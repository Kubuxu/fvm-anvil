use std::{collections::HashMap, time::Duration};

use anvil_rpc::request::RpcMethodCall;
use anvil_server::RpcHandler;
use axum::{
    Json, Router,
    body::Bytes,
    extract::{
        DefaultBodyLimit, State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::{sync::mpsc, task::JoinHandle};
use tower_http::cors::CorsLayer;

use crate::{RpcService, ServerConfig, native};

/// Ethereum/Anvil HTTP plus Lotus-compatible HTTP and WebSocket `/rpc/v1`.
pub fn rpc_router(config: ServerConfig, service: RpcService) -> Router {
    let mut native_router = Router::new()
        .route("/rpc/v1", post(http).get(upgrade))
        .route("/", get(upgrade))
        .with_state(service.clone());
    if !config.no_cors {
        native_router = native_router.layer(
            CorsLayer::new()
                .allow_origin(config.allow_origin.0.clone())
                .allow_headers([axum::http::header::CONTENT_TYPE])
                .allow_methods([axum::http::Method::GET, axum::http::Method::POST]),
        );
    }
    if config.no_request_size_limit {
        native_router = native_router.layer(DefaultBodyLimit::disable());
    }
    anvil_server::http_router(config, service).merge(native_router)
}

fn error(id: Value, code: i64, message: impl Into<String>) -> Value {
    json!({"jsonrpc":"2.0", "id":id, "error":{"code":code,"message":message.into()}})
}

async fn call(service: &RpcService, mut value: Value) -> Option<Value> {
    let notification = value.is_object() && value.get("id").is_none();
    if let Some(object) = value.as_object_mut() {
        // go-jsonrpc's optional OpenCensus propagation is not a method argument.
        object.remove("meta");
        if notification {
            object.insert("id".into(), Value::Null);
        }
    }
    let id = value.get("id").cloned().unwrap_or(Value::Null);
    let response = match serde_json::from_value::<RpcMethodCall>(value) {
        Ok(request) => serde_json::to_value(service.on_call(request).await)
            .expect("RPC responses contain only serializable JSON values"),
        Err(_) => return Some(error(id, -32600, "invalid request")),
    };
    (!notification).then_some(response)
}

async fn http(State(service): State<RpcService>, bytes: Bytes) -> Response {
    let value: Value = match serde_json::from_slice(&bytes) {
        Ok(value) => value,
        Err(_) => return Json(error(Value::Null, -32700, "parse error")).into_response(),
    };
    let response = if let Value::Array(batch) = value {
        if batch.is_empty() {
            Some(error(Value::Null, -32600, "empty batch"))
        } else {
            let mut responses = Vec::new();
            for value in batch {
                if let Some(response) = call(&service, value).await {
                    responses.push(response);
                }
            }
            (!responses.is_empty()).then_some(Value::Array(responses))
        }
    } else {
        call(&service, value).await
    };
    match response {
        Some(value) => Json(value).into_response(),
        None => StatusCode::NO_CONTENT.into_response(),
    }
}

async fn upgrade(State(service): State<RpcService>, ws: WebSocketUpgrade) -> Response {
    ws.on_upgrade(move |socket| connection(service, socket))
}

struct ChannelEvent {
    request: String,
    channel: u64,
    value: Option<Value>,
}

// Abort-on-drop covers connection errors, cancellation, and graceful server shutdown.
#[derive(Default)]
struct Subscriptions(HashMap<String, (u64, JoinHandle<()>)>);
impl Drop for Subscriptions {
    fn drop(&mut self) {
        for (_, task) in self.0.values() {
            task.abort();
        }
    }
}

async fn send(sink: &mut futures::stream::SplitSink<WebSocket, Message>, value: Value) -> bool {
    matches!(
        tokio::time::timeout(
            Duration::from_secs(10),
            sink.send(Message::Text(value.to_string().into()))
        )
        .await,
        Ok(Ok(()))
    )
}

async fn subscribe(
    service: &RpcService,
    value: &Value,
    subscriptions: &mut Subscriptions,
    events: &mpsc::Sender<ChannelEvent>,
    next_channel: &mut u64,
) -> Option<Value> {
    let id = value.get("id").cloned().unwrap_or(Value::Null);
    if id.is_null() {
        return None;
    }
    if value.get("params").is_some_and(|v| !v.is_null() && v != &json!([])) {
        return Some(error(id, -32602, "expected no parameters"));
    }
    if subscriptions.0.len() >= 64 {
        return Some(error(id, -32000, "subscription limit exceeded"));
    }
    let mut receiver = match service.node.subscribe_heads().await {
        Ok(receiver) => receiver,
        Err(err) => return Some(error(id, -32603, err.to_string())),
    };
    let channel = *next_channel;
    let Some(successor) = next_channel.checked_add(1) else {
        return Some(error(id, -32603, "channel ID exhausted"));
    };
    *next_channel = successor;
    let request = id.to_string();
    if let Some((_, old)) = subscriptions.0.remove(&request) {
        old.abort();
    }
    let sender = events.clone();
    let request_copy = request.clone();
    let task = tokio::spawn(async move {
        while let Some(changes) = receiver.recv().await {
            if sender
                .send(ChannelEvent {
                    request: request_copy.clone(),
                    channel,
                    value: Some(native::changes(changes)),
                })
                .await
                .is_err()
            {
                return;
            }
        }
        let _ = sender.send(ChannelEvent { request: request_copy, channel, value: None }).await;
    });
    subscriptions.0.insert(request, (channel, task));
    // Events are drained by the connection only after sending the call/batch response.
    Some(json!({"jsonrpc":"2.0", "id":id, "result":channel}))
}

async fn connection(service: RpcService, socket: WebSocket) {
    let (mut sink, mut stream) = socket.split();
    let (events, mut incoming) = mpsc::channel::<ChannelEvent>(64);
    let mut subscriptions = Subscriptions::default();
    let mut next_channel = 0_u64;
    loop {
        tokio::select! {
            event = incoming.recv() => {
                let Some(event) = event else { break; };
                if subscriptions.0.get(&event.request).is_none_or(|(channel, _)| *channel != event.channel) {
                    continue;
                }
                let value = match event.value {
                    Some(value) => json!({"jsonrpc":"2.0", "method":"xrpc.ch.val", "params":[event.channel, value]}),
                    None => {
                        subscriptions.0.remove(&event.request);
                        json!({"jsonrpc":"2.0", "method":"xrpc.ch.close", "params":[event.channel]})
                    }
                };
                if !send(&mut sink, value).await { break; }
            }
            message = stream.next() => {
                let bytes = match message {
                    Some(Ok(message @ (Message::Text(_) | Message::Binary(_)))) => message.into_data(),
                    Some(Ok(Message::Ping(bytes))) => {
                        if !matches!(tokio::time::timeout(Duration::from_secs(10), sink.send(Message::Pong(bytes))).await, Ok(Ok(()))) { break; }
                        continue;
                    }
                    Some(Ok(Message::Pong(_))) => continue,
                    _ => break,
                };
                let value: Value = match serde_json::from_slice(&bytes) {
                    Ok(value) => value,
                    Err(_) => {
                        if !send(&mut sink, error(Value::Null, -32700, "parse error")).await { break; }
                        continue;
                    }
                };
                let method = value.get("method").and_then(Value::as_str).unwrap_or("");
                if method == "xrpc.cancel" {
                    if let Some(id) = value.get("params").and_then(|v| v.get(0))
                        && let Some((channel, task)) = subscriptions.0.remove(&id.to_string()) {
                        task.abort();
                        if !send(&mut sink, json!({"jsonrpc":"2.0", "method":"xrpc.ch.close", "params":[channel]})).await { break; }
                    }
                    continue;
                }
                if method == "Filecoin.ChainNotify" {
                    if let Some(response) = subscribe(&service, &value, &mut subscriptions, &events, &mut next_channel).await
                        && !send(&mut sink, response).await { break; }
                } else if let Value::Array(batch) = value {
                    if batch.is_empty() {
                        if !send(&mut sink, error(Value::Null, -32600, "empty batch")).await { break; }
                        continue;
                    }
                    let mut responses = Vec::new();
                    for value in batch {
                        let response = if value.get("method").and_then(Value::as_str) == Some("Filecoin.ChainNotify") {
                            subscribe(&service, &value, &mut subscriptions, &events, &mut next_channel).await
                        } else {
                            call(&service, value).await
                        };
                        if let Some(response) = response { responses.push(response); }
                    }
                    if !responses.is_empty() && !send(&mut sink, Value::Array(responses)).await { break; }
                } else if let Some(response) = call(&service, value).await
                    && !send(&mut sink, response).await { break; }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fvm_anvil_node::{ActorBundle, DEFAULT_CHAIN_ID, NodeConfig, NodeService};
    use tokio_tungstenite::{connect_async, tungstenite::Message as ClientMessage};

    async fn receive(
        socket: &mut tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
    ) -> Value {
        let message = tokio::time::timeout(Duration::from_secs(5), socket.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        serde_json::from_slice(&message.into_data()).unwrap()
    }

    #[tokio::test]
    #[ignore = "requires FVM_ANVIL_ACTOR_BUNDLE to point to the official builtin-actors v18 mainnet CAR"]
    async fn go_channel_protocol_cancels_and_reconnects_on_real_transport() {
        let bundle = std::env::var_os("FVM_ANVIL_ACTOR_BUNDLE").unwrap();
        let service = NodeService::start(
            NodeConfig::new(DEFAULT_CHAIN_ID, ActorBundle::local(bundle).unwrap()).unwrap(),
        )
        .unwrap();
        let node = service.handle();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let router = rpc_router(ServerConfig::default(), RpcService::new(node.clone()));
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let url = format!("ws://{address}/rpc/v1");
        let (mut socket, _) = connect_async(&url).await.unwrap();
        socket
            .send(ClientMessage::Text(
                json!({
                    "jsonrpc":"2.0", "id":1, "method":"Filecoin.StateNetworkName",
                    "params":[], "meta":{"SpanContext":""},
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();
        assert_eq!(receive(&mut socket).await["result"], "local-fvm-anvil");
        socket
            .send(ClientMessage::Text(
                json!([
                    {"jsonrpc":"2.0", "id":"head", "method":"Filecoin.ChainNotify", "params":[]},
                    {"jsonrpc":"2.0", "id":"chain", "method":"eth_chainId", "params":[]},
                ])
                .to_string()
                .into(),
            ))
            .await
            .unwrap();
        let responses = receive(&mut socket).await;
        let channel = responses[0]["result"].clone();
        assert_eq!(responses[1]["result"], "0x7a69");
        let current = receive(&mut socket).await;
        assert_eq!(current["method"], "xrpc.ch.val");
        assert_eq!(current["params"][0], channel);
        assert_eq!(current["params"][1][0]["Type"], "current");
        assert_eq!(current["params"][1].as_array().unwrap().len(), 1);
        let snapshot = node.snapshot().await.unwrap();
        node.mine_blocks(1, None, None).await.unwrap();
        let applied = receive(&mut socket).await;
        assert_eq!(applied["params"][1][0]["Type"], "apply");
        assert_eq!(applied["params"][1][0]["Val"]["Height"], 1);
        assert!(node.revert(snapshot).await.unwrap());
        let reverted = receive(&mut socket).await;
        assert_eq!(reverted["params"][1][0]["Type"], "revert");
        assert_eq!(reverted["params"][1][1]["Type"], "apply");
        assert_eq!(reverted["params"][1][1]["Val"]["Height"], 0);
        socket
            .send(ClientMessage::Text(
                json!({
                    "jsonrpc":"2.0", "method":"xrpc.cancel", "params":["head"],
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();
        let closed = receive(&mut socket).await;
        assert_eq!(closed["method"], "xrpc.ch.close");
        assert_eq!(closed["params"][0], channel);
        socket.close(None).await.unwrap();
        let (mut socket, _) = connect_async(&url).await.unwrap();
        socket
            .send(ClientMessage::Text(
                json!({
                    "jsonrpc":"2.0", "id":2, "method":"Filecoin.ChainNotify", "params":[],
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();
        assert_eq!(receive(&mut socket).await["result"], 0);
        let reconnected = receive(&mut socket).await;
        assert_eq!(reconnected["params"][1][0]["Type"], "current");
        assert_eq!(reconnected["params"][1][0]["Val"]["Height"], 0);
        socket
            .send(ClientMessage::Text(
                json!({
                    "jsonrpc":"2.0", "id":3, "method":"eth_chainId", "params":[],
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();
        assert_eq!(receive(&mut socket).await["result"], "0x7a69");
        socket.close(None).await.unwrap();
        server.abort();
        let _ = server.await;
    }
}
