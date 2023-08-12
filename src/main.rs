use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    response::{Html, IntoResponse},
    routing::get,
    Router, TypedHeader,
};

use futures::{sink::SinkExt, stream::StreamExt};
use headers::authorization::Bearer;

use std::{
    collections::HashSet,
    net::SocketAddr,
    str::FromStr,
    sync::{Arc, Mutex},
};
use tokio::sync::broadcast;
use tower_http::trace::{DefaultMakeSpan, TraceLayer};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
// Our shared state
struct AppState {
    // We require unique usernames. This tracks which usernames have been taken.
    user_set: Mutex<HashSet<String>>,
    // Channel used to send messages to all connected clients.
    tx: broadcast::Sender<String>,
}
impl AppState {
    fn broadcast_message(&self, msg: String) {
        let _ = self.tx.send(msg);
    }
}

#[tokio::main]
async fn main() {
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                // ! server refers to the binary name.
                .unwrap_or_else(|_| "server=debug".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    // Set up application state for use with with_state() this is what ensures statefulness
    let user_set = Mutex::new(HashSet::new());
    let (tx, _rx) = broadcast::channel(100);

    let app_state = Arc::new(AppState { user_set, tx });

    let app = Router::new()
        .route("/", get(index))
        .route("/ws", get(websocket_handler))
        .with_state(app_state)
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(DefaultMakeSpan::default().include_headers(true)),
        );

    // tracing::debug!("listening on {}", listener.local_addr().unwrap());

    axum::Server::bind(&SocketAddr::from_str("127.0.0.1:8080").unwrap())
        .serve(app.into_make_service())
        .await
        .unwrap();
}

/// The handler for the HTTP request (this gets called when the HTTP GET lands at the start
/// of websocket negotiation). After this completes, the actual switching from HTTP to
/// websocket protocol will occur.
/// This is the last point where we can extract TCP/IP metadata such as IP address of the client
/// as well as things from HTTP headers such as user-agent of the browser etc.
async fn websocket_handler(
    ws: WebSocketUpgrade,
    current_user: Option<TypedHeader<headers::Authorization<Bearer>>>,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    let token = if let Some(TypedHeader(auth)) = current_user {
        auth.token().to_string()
    } else {
        tracing::info!("Unknown browser Accessed!");
        String::from("Unknown browser")
    };

    println!("User Access Token: `{token:?}` ???");

    ws.on_upgrade(|socket| handle_socket(socket, state))
}

/// This function deals with a single websocket connection, i.e., a single
/// connected client / user, for which we will spawn two independent tasks (for
/// receiving / sending chat messages).
// ! Think of this as broker
async fn handle_socket(stream: WebSocket, state: Arc<AppState>) {
    // By splitting, we can send and receive at the same time.
    let (mut sender, mut receiver) = stream.split();

    // Username gets set in the receive loop, if it's valid.
    let mut username = String::new();

    // Loop until a text message is found.
    while let Some(Ok(message)) = receiver.next().await {
        if let Message::Text(name) = message {
            // If username that is sent by client is not taken, fill username string.
            check_username(&state, &mut username, &name);

            // If not empty we want to quit the loop else we want to quit function.
            if !username.is_empty() {
                break;
            } else {
                // Only send our client that username is taken.
                let _ = sender
                    .send(Message::Text(String::from("Username already taken.")))
                    .await;

                return;
            }
        }
    }
    // We subscribe *before* sending the "joined" message, so that we will also
    // display it to the currently-joining client.
    let mut rx = state.tx.subscribe();

    // Now send the "joined" message to ALL subscribers.
    state.broadcast_message(format!("{} joined", username));

    // Spawn the first task that will receive broadcast messages and send text
    // messages over the websocket to our client.
    let mut send_task = tokio::spawn(async move {
        while let Ok(msg) = rx.recv().await {
            // In any websocket error, break loop.
            if sender.send(Message::Text(msg)).await.is_err() {
                break;
            }
        }
    });

    // Clone things we want to pass (move) to the receiving task.
    let tx = state.tx.clone();
    let name = username.clone();

    // Spawn a task that takes messages from the websocket, prepends the user
    // name, and sends them to all broadcast subscribers.
    let mut recv_task = tokio::spawn(async move {
        'recv_loop: loop {
            match receiver.next().await {
                Some(Ok(Message::Text(text))) => {
                    let _ = tx.send(format!("{}: {}", name, text));
                    continue;
                }
                Some(Ok(Message::Close(_))) => {
                    println!("User closed the connection!");

                    //If user has left, send relevant message and remove the username from user_set
                    let _ = tx.send(format!("{} left.", name));
                    state.user_set.lock().unwrap().remove(&name);
                    break 'recv_loop;
                }
                _ => break 'recv_loop,
            }
        }
    });

    // If any one of the tasks run to completion, we abort the other.
    tokio::select! {
        _ = (&mut send_task) => recv_task.abort(),
        _ = (&mut recv_task) => send_task.abort(),
    };

    // Remove username from map so new clients can take it again.
}

fn check_username(state: &AppState, string: &mut String, name: &str) {
    let mut user_set = state.user_set.lock().unwrap();

    if !user_set.contains(name) {
        user_set.insert(name.to_owned());

        string.push_str(name);
    }
}

// Include utf-8 file at **compile** time.
async fn index() -> Html<&'static str> {
    Html(std::include_str!("../chat.html"))
}
