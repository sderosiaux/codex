use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicI64;
use std::sync::atomic::Ordering;
use std::time::Duration;

use codex_exec_server_protocol::JSONRPCError;
use codex_exec_server_protocol::JSONRPCErrorError;
use codex_exec_server_protocol::JSONRPCMessage;
use codex_exec_server_protocol::JSONRPCNotification;
use codex_exec_server_protocol::JSONRPCRequest;
use codex_exec_server_protocol::JSONRPCResponse;
use codex_exec_server_protocol::RequestId;
use codex_otel::MetricsClient;
use codex_protocol::protocol::W3cTraceContext;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use tokio::sync::Semaphore;
use tokio::sync::SemaphorePermit;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::timeout;

use crate::client_telemetry::record_client_request;
use crate::connection::JsonRpcConnection;
use crate::connection::JsonRpcConnectionEvent;
use crate::connection::JsonRpcTransport;
use crate::rpc_timing::RpcCompletion;

pub(crate) const SESSION_ALREADY_ATTACHED_ERROR_CODE: i64 = -32010;
const MAX_IN_FLIGHT_REGULAR_CALLS: usize = 1024;
const RESERVED_CLEANUP_CALLS: usize = 1;

#[derive(Debug)]
pub(crate) enum RpcCallError {
    /// The underlying JSON-RPC transport closed before this call completed.
    Closed,
    /// The response bytes were valid JSON-RPC but not the expected result type.
    Json(serde_json::Error),
    /// The executor returned a JSON-RPC error response for this call.
    Server(JSONRPCErrorError),
    /// The executor did not return a response before the caller's deadline.
    TimedOut { method: String, timeout: Duration },
    /// The client already has the maximum number of regular RPC calls in flight.
    PendingRequestLimitExceeded { limit: usize },
}

#[path = "rpc_pending_request.rs"]
mod pending_request;
use pending_request::PendingRequestGuard;

type PendingRequest = oneshot::Sender<RpcCompletion>;
type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;
type RequestRoute<S> = Box<
    dyn Fn(Arc<S>, JSONRPCRequest) -> BoxFuture<Option<RpcServerOutboundMessage>> + Send + Sync,
>;
type NotificationRoute<S> =
    Box<dyn Fn(Arc<S>, JSONRPCNotification) -> BoxFuture<Result<(), String>> + Send + Sync>;

enum RpcCallTimeout {
    None,
    After(Duration),
}

#[derive(Debug)]
pub(crate) enum RpcClientEvent {
    Request {
        request: JSONRPCRequest,
        request_span: tracing::Span,
    },
    Notification(JSONRPCNotification),
    Disconnected {
        reason: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum RpcServerOutboundMessage {
    Response {
        request_id: RequestId,
        result: Value,
    },
    Error {
        request_id: RequestId,
        error: JSONRPCErrorError,
    },
    Notification(JSONRPCNotification),
}

#[derive(Clone)]
pub(crate) struct RpcNotificationSender {
    outgoing_tx: mpsc::Sender<RpcServerOutboundMessage>,
}

impl RpcNotificationSender {
    pub(crate) fn new(outgoing_tx: mpsc::Sender<RpcServerOutboundMessage>) -> Self {
        Self { outgoing_tx }
    }

    pub(crate) async fn response(
        &self,
        request_id: RequestId,
        result: Value,
    ) -> Result<(), JSONRPCErrorError> {
        self.outgoing_tx
            .send(RpcServerOutboundMessage::Response { request_id, result })
            .await
            .map_err(|_| internal_error("RPC connection closed while sending response".into()))
    }

    pub(crate) async fn notify<P: Serialize>(
        &self,
        method: &str,
        params: &P,
    ) -> Result<(), JSONRPCErrorError> {
        let params = serde_json::to_value(params).map_err(|err| internal_error(err.to_string()))?;
        self.outgoing_tx
            .send(RpcServerOutboundMessage::Notification(
                JSONRPCNotification {
                    method: method.to_string(),
                    params: Some(params),
                },
            ))
            .await
            .map_err(|_| internal_error("RPC connection closed while sending notification".into()))
    }
}

pub(crate) struct RpcRouter<S> {
    request_routes: HashMap<&'static str, RequestRoute<S>>,
    notification_routes: HashMap<&'static str, NotificationRoute<S>>,
}

impl<S> Default for RpcRouter<S> {
    fn default() -> Self {
        Self {
            request_routes: HashMap::new(),
            notification_routes: HashMap::new(),
        }
    }
}

impl<S> RpcRouter<S>
where
    S: Send + Sync + 'static,
{
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn request<P, R, F, Fut>(&mut self, method: &'static str, handler: F)
    where
        P: DeserializeOwned + Send + 'static,
        R: Serialize + Send + 'static,
        F: Fn(Arc<S>, P) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<R, JSONRPCErrorError>> + Send + 'static,
    {
        self.request_with_trace(method, move |state, params, _trace| handler(state, params));
    }

    /// Supplies the incoming W3C carrier to handlers that need it without requiring a trace exporter.
    pub(crate) fn request_with_trace<P, R, F, Fut>(&mut self, method: &'static str, handler: F)
    where
        P: DeserializeOwned + Send + 'static,
        R: Serialize + Send + 'static,
        F: Fn(Arc<S>, P, Option<W3cTraceContext>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<R, JSONRPCErrorError>> + Send + 'static,
    {
        self.request_routes.insert(
            method,
            Box::new(move |state, request| {
                let trace = request.trace;
                let request_id = request.id;
                let params = request.params;
                let response =
                    decode_request_params::<P>(params).map(|params| handler(state, params, trace));
                Box::pin(async move {
                    let response = match response {
                        Ok(response) => response.await,
                        Err(error) => {
                            return Some(RpcServerOutboundMessage::Error { request_id, error });
                        }
                    };
                    Some(match response {
                        Ok(result) => match serde_json::to_value(result) {
                            Ok(result) => RpcServerOutboundMessage::Response { request_id, result },
                            Err(err) => RpcServerOutboundMessage::Error {
                                request_id,
                                error: internal_error(err.to_string()),
                            },
                        },
                        Err(error) => RpcServerOutboundMessage::Error { request_id, error },
                    })
                })
            }),
        );
    }

    pub(crate) fn request_with_id<P, F, Fut>(&mut self, method: &'static str, handler: F)
    where
        P: DeserializeOwned + Send + 'static,
        F: Fn(Arc<S>, RequestId, P) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<(), JSONRPCErrorError>> + Send + 'static,
    {
        self.request_routes.insert(
            method,
            Box::new(move |state, request| {
                let request_id = request.id;
                let params = decode_request_params::<P>(request.params)
                    .map(|params| handler(state, request_id.clone(), params));
                Box::pin(async move {
                    let response = match params {
                        Ok(response) => response.await,
                        Err(error) => {
                            return Some(RpcServerOutboundMessage::Error { request_id, error });
                        }
                    };
                    match response {
                        Ok(()) => None,
                        Err(error) => Some(RpcServerOutboundMessage::Error { request_id, error }),
                    }
                })
            }),
        );
    }

    pub(crate) fn notification<P, F, Fut>(&mut self, method: &'static str, handler: F)
    where
        P: DeserializeOwned + Send + 'static,
        F: Fn(Arc<S>, P) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<(), String>> + Send + 'static,
    {
        self.notification_routes.insert(
            method,
            Box::new(move |state, notification| {
                let params = decode_notification_params::<P>(notification.params)
                    .map(|params| handler(state, params));
                Box::pin(async move {
                    let handler = match params {
                        Ok(handler) => handler,
                        Err(err) => return Err(err),
                    };
                    handler.await
                })
            }),
        );
    }

    pub(crate) fn request_route(&self, method: &str) -> Option<(&'static str, &RequestRoute<S>)> {
        self.request_routes
            .get_key_value(method)
            .map(|(&method, route)| (method, route))
    }

    pub(crate) fn notification_route(&self, method: &str) -> Option<&NotificationRoute<S>> {
        self.notification_routes.get(method)
    }
}

pub(crate) struct RpcClient {
    metrics: Option<MetricsClient>,
    write_tx: mpsc::Sender<JSONRPCMessage>,
    pending: Arc<StdMutex<HashMap<RequestId, PendingRequest>>>,
    // Shared transport state from `JsonRpcConnection`. Calls use this to fail
    // immediately when the socket closes, even if no JSON-RPC error response
    // can be delivered for their request id.
    disconnected_rx: watch::Receiver<bool>,
    closed: Arc<AtomicBool>,
    shared_call_slots: Semaphore,
    cleanup_call_slots: Semaphore,
    next_request_id: AtomicI64,
    transport_tasks: Vec<JoinHandle<()>>,
    transport: JsonRpcTransport,
    reader_task: JoinHandle<()>,
}

impl RpcClient {
    pub(crate) fn new(connection: JsonRpcConnection) -> (Self, mpsc::Receiver<RpcClientEvent>) {
        let JsonRpcConnection {
            outgoing_tx: write_tx,
            mut incoming_rx,
            disconnected_rx,
            task_handles: transport_tasks,
            transport,
        } = connection;
        let pending = Arc::new(StdMutex::new(HashMap::<RequestId, PendingRequest>::new()));
        let closed = Arc::new(AtomicBool::new(false));
        let (event_tx, event_rx) = mpsc::channel(128);

        let pending_for_reader = Arc::clone(&pending);
        let closed_for_reader = Arc::clone(&closed);
        let transport_for_reader = transport.clone();
        let reader_task = tokio::spawn(async move {
            let disconnect_reason = loop {
                let Some(event) = incoming_rx.recv().await else {
                    break None;
                };
                match event {
                    JsonRpcConnectionEvent::Message(message) => {
                        if let Err(err) =
                            handle_server_message(&pending_for_reader, &event_tx, message).await
                        {
                            let _ = err;
                            break None;
                        }
                    }
                    JsonRpcConnectionEvent::QueuedRequest {
                        request,
                        request_span,
                        ..
                    } => {
                        if event_tx
                            .send(RpcClientEvent::Request {
                                request,
                                request_span,
                            })
                            .await
                            .is_err()
                        {
                            break None;
                        }
                    }
                    JsonRpcConnectionEvent::MalformedMessage { reason } => {
                        let _ = reason;
                        break None;
                    }
                    JsonRpcConnectionEvent::Disconnected { reason } => {
                        break reason;
                    }
                }
            };

            closed_for_reader.store(true, Ordering::Release);
            drain_pending(&pending_for_reader);
            let _ = event_tx
                .send(RpcClientEvent::Disconnected {
                    reason: disconnect_reason,
                })
                .await;
            transport_for_reader.terminate();
        });

        (
            Self {
                metrics: codex_otel::global(),
                write_tx,
                pending,
                disconnected_rx,
                closed,
                shared_call_slots: Semaphore::new(MAX_IN_FLIGHT_REGULAR_CALLS),
                cleanup_call_slots: Semaphore::new(RESERVED_CLEANUP_CALLS),
                next_request_id: AtomicI64::new(1),
                transport_tasks,
                transport,
                reader_task,
            },
            event_rx,
        )
    }

    pub(crate) async fn notify<P: Serialize>(
        &self,
        method: &str,
        params: &P,
    ) -> Result<(), RpcCallError> {
        let params = serde_json::to_value(params).map_err(RpcCallError::Json)?;
        if self.closed.load(Ordering::Acquire) || *self.disconnected_rx.borrow() {
            return Err(RpcCallError::Closed);
        }
        self.write_tx
            .send(JSONRPCMessage::Notification(JSONRPCNotification {
                method: method.to_string(),
                params: Some(params),
            }))
            .await
            .map_err(|_| RpcCallError::Closed)
    }

    pub(crate) async fn respond_error(
        &self,
        request_id: RequestId,
        error: JSONRPCErrorError,
    ) -> Result<(), RpcCallError> {
        if self.closed.load(Ordering::Acquire) || *self.disconnected_rx.borrow() {
            return Err(RpcCallError::Closed);
        }
        self.write_tx
            .send(JSONRPCMessage::Error(JSONRPCError {
                id: request_id,
                error,
            }))
            .await
            .map_err(|_| RpcCallError::Closed)
    }

    pub(crate) fn is_disconnected(&self) -> bool {
        self.closed.load(Ordering::Acquire) || *self.disconnected_rx.borrow()
    }

    pub(crate) async fn close_transport(&self) {
        self.closed.store(true, Ordering::Release);
        self.transport.terminate();
        for task in &self.transport_tasks {
            task.abort();
        }
        drain_pending(&self.pending);
    }

    // Callers keep this permit until `call_inner` returns, so an executor
    // cannot free admission early by guessing a request id and replying before
    // the request leaves the outbound queue.
    fn acquire_regular_call_slot(&self) -> Result<SemaphorePermit<'_>, RpcCallError> {
        self.shared_call_slots.try_acquire().map_err(|_| {
            RpcCallError::PendingRequestLimitExceeded {
                limit: MAX_IN_FLIGHT_REGULAR_CALLS,
            }
        })
    }

    #[tracing::instrument(
        name = "codex.exec_server.request",
        level = "info",
        skip_all,
        fields(
            otel.kind = "client",
            otel.name = method,
            method,
        )
    )]
    pub(crate) async fn call<P, T>(&self, method: &str, params: &P) -> Result<T, RpcCallError>
    where
        P: Serialize,
        T: DeserializeOwned,
    {
        self.call_untraced(method, params).await
    }

    /// Send one request without creating the standard request span.
    ///
    /// Callers use this only when they install a more precise request span
    /// around the same wire operation.
    pub(crate) async fn call_untraced<P, T>(
        &self,
        method: &str,
        params: &P,
    ) -> Result<T, RpcCallError>
    where
        P: Serialize,
        T: DeserializeOwned,
    {
        record_client_request(self.metrics.as_ref(), method);
        let _call_slot = self.acquire_regular_call_slot()?;
        self.call_inner(method, params, RpcCallTimeout::None).await
    }

    pub(crate) async fn call_with_timeout<P, T>(
        &self,
        method: &str,
        params: &P,
        call_timeout: Duration,
    ) -> Result<T, RpcCallError>
    where
        P: Serialize,
        T: DeserializeOwned,
    {
        record_client_request(self.metrics.as_ref(), method);
        let _call_slot = self.acquire_regular_call_slot()?;
        self.call_inner(method, params, RpcCallTimeout::After(call_timeout))
            .await
    }

    #[tracing::instrument(
        name = "codex.exec_server.request",
        level = "info",
        skip_all,
        fields(
            otel.kind = "client",
            otel.name = method,
            method,
        )
    )]
    pub(crate) async fn call_for_cleanup<P, T>(
        &self,
        method: &str,
        params: &P,
    ) -> Result<T, RpcCallError>
    where
        P: Serialize,
        T: DeserializeOwned,
    {
        record_client_request(self.metrics.as_ref(), method);
        let _call_slot = match self.shared_call_slots.try_acquire() {
            Ok(call_slot) => call_slot,
            Err(_) => match self.cleanup_call_slots.try_acquire() {
                Ok(call_slot) => call_slot,
                Err(_) => {
                    self.close_transport().await;
                    return Err(RpcCallError::Closed);
                }
            },
        };
        self.call_inner(method, params, RpcCallTimeout::None).await
    }

    async fn call_inner<P, T>(
        &self,
        method: &str,
        params: &P,
        call_timeout: RpcCallTimeout,
    ) -> Result<T, RpcCallError>
    where
        P: Serialize,
        T: DeserializeOwned,
    {
        let request_id = RequestId::Integer(self.next_request_id.fetch_add(1, Ordering::SeqCst));
        let (response_tx, response_rx) = oneshot::channel();
        {
            let mut pending = self
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            // Registering the pending request and checking disconnect must be
            // atomic with the reader's drain_pending path. Otherwise a call
            // can sneak in after the drain and wait forever.
            if self.closed.load(Ordering::Acquire) || *self.disconnected_rx.borrow() {
                return Err(RpcCallError::Closed);
            }
            pending.insert(request_id.clone(), response_tx);
        }

        let _pending = PendingRequestGuard {
            pending: &self.pending,
            request_id: request_id.clone(),
        };
        let params = serde_json::to_value(params).map_err(RpcCallError::Json)?;
        let response = self
            .send_request_and_wait(method, request_id, params, response_rx, call_timeout)
            .await?;
        serde_json::from_value(response).map_err(RpcCallError::Json)
    }

    // Sending and waiting are independent of P and T, so share this future
    // across typed calls. Register and serialize in call_inner in that order:
    // a disconnected client must still take precedence over a serialization error.
    async fn send_request_and_wait(
        &self,
        method: &str,
        request_id: RequestId,
        params: Value,
        response_rx: oneshot::Receiver<RpcCompletion>,
        call_timeout: RpcCallTimeout,
    ) -> Result<Value, RpcCallError> {
        if self
            .write_tx
            .send(JSONRPCMessage::Request(JSONRPCRequest {
                id: request_id.clone(),
                method: method.to_string(),
                params: Some(params),
                trace: codex_otel::current_span_w3c_trace_context(),
            }))
            .await
            .is_err()
        {
            return Err(RpcCallError::Closed);
        }
        tracing::event!(
            name: "codex.exec_server.request_enqueued",
            target: "codex_otel.trace_safe",
            tracing::Level::INFO,
            event.name = "codex.exec_server.request_enqueued",
            rpc.method = method,
        );
        // Do not race in-flight requests directly against the transport-close
        // watch value. The connection reader receives JSON-RPC messages and
        // the terminal disconnect event on one ordered queue, then drains any
        // still-pending requests. Awaiting this receiver preserves that order:
        // responses already read before EOF still win, and truly pending calls
        // are failed once the reader observes the disconnect.
        let response = match call_timeout {
            RpcCallTimeout::None => response_rx.await,
            RpcCallTimeout::After(call_timeout) => match timeout(call_timeout, response_rx).await {
                Ok(response) => response,
                Err(_) => {
                    return Err(RpcCallError::TimedOut {
                        method: method.to_string(),
                        timeout: call_timeout,
                    });
                }
            },
        };
        let completion = response.map_err(|_| RpcCallError::Closed)?;
        completion.record_receipt(method);
        completion.result
    }
}

impl Drop for RpcClient {
    fn drop(&mut self) {
        self.transport.terminate();
        for task in &self.transport_tasks {
            task.abort();
        }
        self.reader_task.abort();
    }
}

pub(crate) fn encode_server_message(
    message: RpcServerOutboundMessage,
) -> Result<JSONRPCMessage, serde_json::Error> {
    match message {
        RpcServerOutboundMessage::Response { request_id, result } => {
            Ok(JSONRPCMessage::Response(JSONRPCResponse {
                id: request_id,
                result,
            }))
        }
        RpcServerOutboundMessage::Error { request_id, error } => {
            Ok(JSONRPCMessage::Error(JSONRPCError {
                id: request_id,
                error,
            }))
        }
        RpcServerOutboundMessage::Notification(notification) => {
            Ok(JSONRPCMessage::Notification(notification))
        }
    }
}

pub(crate) fn invalid_request(message: String) -> JSONRPCErrorError {
    JSONRPCErrorError {
        code: -32600,
        data: None,
        message,
    }
}

pub(crate) fn session_already_attached(message: String) -> JSONRPCErrorError {
    JSONRPCErrorError {
        code: SESSION_ALREADY_ATTACHED_ERROR_CODE,
        data: None,
        message,
    }
}

pub(crate) fn method_not_found(message: String) -> JSONRPCErrorError {
    JSONRPCErrorError {
        code: -32601,
        data: None,
        message,
    }
}

pub(crate) fn invalid_params(message: String) -> JSONRPCErrorError {
    JSONRPCErrorError {
        code: -32602,
        data: None,
        message,
    }
}

pub(crate) fn not_found(message: String) -> JSONRPCErrorError {
    JSONRPCErrorError {
        code: -32004,
        data: None,
        message,
    }
}

pub(crate) fn internal_error(message: String) -> JSONRPCErrorError {
    JSONRPCErrorError {
        code: -32603,
        data: None,
        message,
    }
}

fn decode_request_params<P>(params: Option<Value>) -> Result<P, JSONRPCErrorError>
where
    P: DeserializeOwned,
{
    decode_params(params).map_err(|err| invalid_params(err.to_string()))
}

fn decode_notification_params<P>(params: Option<Value>) -> Result<P, String>
where
    P: DeserializeOwned,
{
    decode_params(params).map_err(|err| err.to_string())
}

fn decode_params<P>(params: Option<Value>) -> Result<P, serde_json::Error>
where
    P: DeserializeOwned,
{
    let params = params.unwrap_or(Value::Null);
    let retry_as_null = matches!(&params, Value::Object(map) if map.is_empty());
    match serde_json::from_value(params) {
        Ok(params) => Ok(params),
        Err(err) => {
            if retry_as_null {
                serde_json::from_value(Value::Null).map_err(|_| err)
            } else {
                Err(err)
            }
        }
    }
}

async fn handle_server_message(
    pending: &StdMutex<HashMap<RequestId, PendingRequest>>,
    event_tx: &mpsc::Sender<RpcClientEvent>,
    message: JSONRPCMessage,
) -> Result<(), String> {
    match message {
        JSONRPCMessage::Response(JSONRPCResponse { id, result }) => {
            if let Some(pending) = pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&id)
            {
                let _ = pending.send(RpcCompletion::new(Ok(result)));
            }
        }
        JSONRPCMessage::Error(JSONRPCError { id, error }) => {
            if let Some(pending) = pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&id)
            {
                let _ = pending.send(RpcCompletion::new(Err(RpcCallError::Server(error))));
            }
        }
        JSONRPCMessage::Notification(notification) => {
            let _ = event_tx
                .send(RpcClientEvent::Notification(notification))
                .await;
        }
        JSONRPCMessage::Request(request) => {
            event_tx
                .send(RpcClientEvent::Request {
                    request,
                    request_span: tracing::Span::none(),
                })
                .await
                .map_err(|_| "RPC client event receiver closed".to_string())?;
        }
    }

    Ok(())
}

fn drain_pending(pending: &StdMutex<HashMap<RequestId, PendingRequest>>) {
    let pending = {
        let mut pending = pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        pending
            .drain()
            .map(|(_, pending)| pending)
            .collect::<Vec<_>>()
    };
    for pending in pending {
        let _ = pending.send(RpcCompletion::new(Err(RpcCallError::Closed)));
    }
}
