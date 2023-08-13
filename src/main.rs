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
    net::SocketAddr,
    str::FromStr,
    sync::{Arc, Mutex},
};
use tokio::sync::broadcast::{self, Sender};
use tower_http::trace::{DefaultMakeSpan, TraceLayer};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

// Our shared state

struct Room {
    id: String,
    tx: broadcast::Sender<String>,
}

#[derive(Default)]
struct AppState {
    rooms: Vec<Room>,
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

    let app_state = Arc::new(Mutex::new(AppState::default()));

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
    State(state): State<Arc<Mutex<AppState>>>,
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
async fn handle_socket(stream: WebSocket, state: Arc<Mutex<AppState>>) {
    // By splitting, we can send and receive at the same time.
    let (mut sender, mut receiver) = stream.split();

    // Username gets set in the receive loop, if it's valid.
    let mut user_name = String::new();
    let mut room: Option<Sender<String>> = None;
    // Loop until a text message is found.
    while let Some(Ok(message)) = receiver.next().await {
        if let Message::Text(enter_key) = message {
            // If username that is sent by client is not taken, fill username string.

            let mut iterator = enter_key.split(':');
            let (r_name, u_name) = (iterator.next().unwrap_or(""), iterator.next().unwrap_or(""));

            room = Some(check_room(state, r_name));

            // If not empty we want to quit the loop else we want to quit function.
            if ![!u_name.is_empty(), !r_name.is_empty()].contains(&false) {
                user_name = u_name.to_string();
                break;
            } else {
                // Only send our client that username is taken.
                let _ = sender
                    .send(Message::Text(String::from("Wrong input is given.")))
                    .await;

                return;
            }
        }
    }
    // We subscribe *before* sending the "joined" message, so that we will also
    // display it to the currently-joining client.
    let room = room.take().unwrap();
    let mut rx = room.subscribe();

    // Now send the "joined" message to ALL subscribers.
    let _ = room.send(format!("{} joined", user_name));

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
    let tx = room.clone();
    let name = user_name.clone();

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

fn check_room(state: Arc<Mutex<AppState>>, room_name: &str) -> Sender<String> {
    let mut state = state.lock().unwrap();

    let rooms = &mut state.rooms;

    if !rooms.iter().any(|r| r.id == room_name) {
        let (tx, _rx) = broadcast::channel(100);

        rooms.push(Room {
            id: room_name.to_string(),
            tx: tx.clone(),
        });
        tx
    } else {
        rooms.iter().find(|r| r.id == room_name).unwrap().tx.clone()
    }
}

// Include utf-8 file at **compile** time.
async fn index() -> Html<&'static str> {
    Html(std::include_str!("../chat.html"))
}
