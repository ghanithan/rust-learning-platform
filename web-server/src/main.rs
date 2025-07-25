use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Path as AxumPath, Query, State,
    },
    http::{header, HeaderValue, Method, StatusCode},
    response::{Html, IntoResponse, Response},
    routing::{get, post, put},
    Json, Router,
};
use chrono::{DateTime, Utc};
use futures_util::{sink::SinkExt, stream::StreamExt};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    env,
    io::{Read, Write},
    net::SocketAddr,
    path::PathBuf,
    process::Stdio,
    sync::Arc,
    thread,
    time::Duration,
};
use tokio::{
    fs,
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::Command,
    sync::{broadcast, RwLock, Mutex},
    time::timeout,
};
use tower::ServiceBuilder;
use tower_http::{
    compression::CompressionLayer,
    cors::{Any, CorsLayer},
    limit::RequestBodyLimitLayer,
    trace::TraceLayer,
};
use tracing::{debug, error, info, warn};
use uuid::Uuid;
use walkdir::WalkDir;

#[cfg(feature = "embed-assets")]
use rust_embed::RustEmbed;

#[cfg(feature = "embed-assets")]
#[derive(RustEmbed)]
#[folder = "../web/dist/"]
struct Assets;

#[cfg(feature = "embed-assets")]
#[derive(RustEmbed)]
#[folder = "../web/node_modules/monaco-editor/"]
struct MonacoAssets;

// Application state
#[derive(Clone)]
struct AppState {
    connections: Arc<RwLock<HashSet<ConnectionId>>>,
    terminal_sessions: Arc<RwLock<HashMap<String, TerminalSession>>>,
    pty_handles: Arc<RwLock<HashMap<String, PtyHandle>>>,
    broadcast_tx: broadcast::Sender<BroadcastMessage>,
    debug_websocket: bool,
    exercises_path: PathBuf,
    progress_path: PathBuf,
}

type ConnectionId = Uuid;

#[derive(Debug, Clone)]
struct TerminalSession {
    session_id: String,
    connection_id: ConnectionId,
}

// Separate struct for actual PTY handles (not Clone/Send)
struct PtyHandle {
    writer: Arc<Mutex<Box<dyn std::io::Write + Send>>>,
    child: Arc<Mutex<Box<dyn portable_pty::Child + Send + Sync>>>,
    master: Arc<Mutex<Box<dyn portable_pty::MasterPty + Send>>>,
}

#[derive(Debug, Clone, Serialize)]
struct BroadcastMessage {
    #[serde(rename = "type")]
    msg_type: String,
    #[serde(flatten)]
    data: serde_json::Value,
}

// WebSocket message types
#[derive(Debug, Deserialize)]
struct WebSocketMessage {
    #[serde(rename = "type")]
    msg_type: String,
    #[serde(flatten)]
    data: serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct TerminalMessage {
    action: String,
    #[serde(rename = "sessionId")]
    session_id: Option<String>,
    input: Option<String>,
    cols: Option<u16>,
    rows: Option<u16>,
}

// API response types
#[derive(Debug, Serialize)]
struct CargoResult {
    success: bool,
    code: Option<i32>,
    stdout: String,
    stderr: String,
    output: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct ExerciseMetadata {
    id: String,
    title: String,
    description: String,
    chapter: u32,
    exercise_number: u32,
    difficulty: String,
    estimated_time_minutes: u32,
    concepts: Vec<String>,
    prerequisites: Vec<String>,
    exercise_type: String,
    #[serde(default)]
    rust_book_refs: serde_json::Value,
    #[serde(default)]
    hints: serde_json::Value,
    #[serde(default)]
    testing: serde_json::Value,
    #[serde(default)]
    validation: serde_json::Value,
}

#[derive(Debug, Serialize)]
struct ExerciseWithPath {
    #[serde(flatten)]
    metadata: ExerciseMetadata,
    path: String,
}

#[derive(Debug, Serialize)]
struct ExerciseDetails {
    metadata: ExerciseMetadata,
    #[serde(rename = "mainContent")]
    main_content: String,
    readme: String,
    hints: String,
    path: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct ProgressData {
    user_id: String,
    created_at: String,
    overall_progress: f64,
    chapters_completed: u32,
    exercises_completed: u32,
    total_exercises: u32,
    current_streak: u32,
    longest_streak: u32,
    total_time_minutes: u32,
    chapters: serde_json::Value,
    exercise_history: Vec<ExerciseHistoryEntry>,
    achievements: Vec<serde_json::Value>,
    session_stats: SessionStats,
}

#[derive(Debug, Serialize, Deserialize)]
struct ExerciseHistoryEntry {
    exercise_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    viewed_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    completed_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    time_taken_minutes: Option<u32>,
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    hints_used: Option<Vec<u32>>,
}

#[derive(Debug, Serialize, Deserialize)]
struct SessionStats {
    exercises_viewed: u32,
    exercises_completed: u32,
    hints_used: u32,
    time_spent: u32,
}

#[derive(Debug, Deserialize)]
struct SaveCodeRequest {
    code: String,
}

#[derive(Debug, Deserialize)]
struct CompleteExerciseRequest {
    exercise_id: String,
    time_taken_minutes: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct HintRequest {
    exercise_id: String,
    hint_level: u32,
}

#[derive(Debug, Deserialize)]
struct ViewRequest {
    exercise_id: String,
}

#[derive(Debug, Serialize)]
struct BookResponse {
    url: String,
    chapter: String,
}

#[derive(Debug, Serialize)]
struct ApiResponse<T> {
    #[serde(skip_serializing_if = "Option::is_none")]
    success: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(flatten)]
    data: Option<T>,
    #[serde(flatten)]
    extra: Option<serde_json::Value>,
}

impl<T> ApiResponse<T> {
    fn success(data: T) -> Self {
        Self {
            success: Some(true),
            message: None,
            error: None,
            data: Some(data),
            extra: None,
        }
    }

    fn success_with_extra(data: T, extra: serde_json::Value) -> Self {
        Self {
            success: Some(true),
            message: None,
            error: None,
            data: Some(data),
            extra: Some(extra),
        }
    }

    fn error(message: String) -> ApiResponse<()> {
        ApiResponse {
            success: Some(false),
            message: None,
            error: Some(message),
            data: None,
            extra: None,
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "rust_tour=info,tower_http=debug".into()),
        )
        .init();

    let port = env::var("PORT")
        .unwrap_or_else(|_| "3000".to_string())
        .parse::<u16>()
        .unwrap_or(3000);

    let debug_websocket = env::var("DEBUG_WEBSOCKET")
        .map(|v| v == "true")
        .unwrap_or(false);

    // Set up paths
    let current_dir = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let exercises_path = current_dir.join("exercises");
    let progress_path = current_dir.join("progress").join("user_progress.json");

    // Create broadcast channel for WebSocket messages
    let (broadcast_tx, _) = broadcast::channel(100);

    // Initialize application state
    let state = AppState {
        connections: Arc::new(RwLock::new(HashSet::new())),
        terminal_sessions: Arc::new(RwLock::new(HashMap::new())),
        pty_handles: Arc::new(RwLock::new(HashMap::new())),
        broadcast_tx: broadcast_tx.clone(),
        debug_websocket,
        exercises_path: exercises_path.clone(),
        progress_path,
    };

    // Initialize progress system
    initialize_progress_system(&state).await?;

    // Set up file watching
    setup_file_watcher(state.clone()).await?;

    // Build the application router
    let app = create_router(state);

    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    let listener = tokio::net::TcpListener::bind(addr).await?;

    info!("🌐 Rust Tour server running on http://localhost:{}", port);
    info!("📡 WebSocket available at ws://localhost:{}/ws", port);
    info!("🦀 Ready to serve Rust tutorial exercises!");

    axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>())
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    Ok(())
}

fn create_router(state: AppState) -> Router {
    Router::new()
        // WebSocket route
        .route("/ws", get(websocket_handler))
        
        // API routes
        .route("/api/exercises", get(get_exercises))
        .route("/api/exercises/:chapter/:exercise", get(get_exercise))
        .route("/api/exercises/:chapter/:exercise/code", put(save_exercise_code))
        .route("/api/exercises/:chapter/:exercise/test", post(test_exercise))
        .route("/api/exercises/:chapter/:exercise/run", post(run_exercise))
        .route("/api/exercises/:chapter/:exercise/check", post(check_exercise))
        .route("/api/progress", get(get_progress))
        .route("/api/progress/complete", post(complete_exercise))
        .route("/api/progress/hint", post(track_hint_usage))
        .route("/api/progress/view", post(track_exercise_view))
        .route("/api/book/:chapter", get(get_book_chapter))
        
        // Static file routes
        .route("/monaco/*path", get(serve_monaco_files))
        .fallback(serve_static_files)
        
        .layer(
            ServiceBuilder::new()
                .layer(TraceLayer::new_for_http())
                .layer(CompressionLayer::new())
                .layer(RequestBodyLimitLayer::new(50 * 1024 * 1024)) // 50MB limit
                .layer(
                    CorsLayer::new()
                        .allow_methods([Method::GET, Method::POST, Method::PUT, Method::DELETE])
                        .allow_headers(Any)
                        .allow_origin(Any),
                )
        )
        .with_state(state)
}

// WebSocket handlers
async fn websocket_handler(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
) -> Response {
    ws.on_upgrade(|socket| websocket_connection(socket, state))
}

async fn websocket_connection(socket: WebSocket, state: AppState) {
    let connection_id = Uuid::new_v4();
    
    // Add connection to state
    {
        let mut connections = state.connections.write().await;
        connections.insert(connection_id);
    }
    
    info!("Client connected to WebSocket: {}", connection_id);
    
    let mut broadcast_rx = state.broadcast_tx.subscribe();
    let (mut sender, mut receiver) = socket.split();
    
    // Spawn task to handle broadcast messages
    let broadcast_state = state.clone();
    let broadcast_task = tokio::spawn(async move {
        while let Ok(msg) = broadcast_rx.recv().await {
            if let Ok(json) = serde_json::to_string(&msg) {
                if sender.send(Message::Text(json)).await.is_err() {
                    break;
                }
            }
        }
    });
    
    // Handle incoming messages
    while let Some(msg) = receiver.next().await {
        match msg {
            Ok(Message::Text(text)) => {
                if let Err(e) = handle_websocket_message(text, &state, connection_id).await {
                    error!("Error handling WebSocket message: {}", e);
                }
            }
            Ok(Message::Close(_)) => {
                break;
            }
            Err(e) => {
                error!("WebSocket error: {}", e);
                break;
            }
            _ => {}
        }
    }
    
    // Cleanup on disconnect
    {
        let mut connections = state.connections.write().await;
        connections.remove(&connection_id);
    }
    
    // Clean up terminal sessions for this connection
    cleanup_terminal_sessions(&state, connection_id).await;
    
    broadcast_task.abort();
    info!("Client disconnected from WebSocket: {}", connection_id);
}

async fn handle_websocket_message(
    text: String,
    state: &AppState,
    connection_id: ConnectionId,
) -> anyhow::Result<()> {
    let message: WebSocketMessage = serde_json::from_str(&text)?;
    
    if state.debug_websocket {
        debug!("Received WebSocket message: {}", message.msg_type);
    }
    
    match message.msg_type.as_str() {
        "terminal" => {
            let terminal_msg: TerminalMessage = serde_json::from_value(message.data)?;
            handle_terminal_message(state, connection_id, terminal_msg).await?;
        }
        _ => {
            warn!("Unknown WebSocket message type: {}", message.msg_type);
        }
    }
    
    Ok(())
}

async fn handle_terminal_message(
    state: &AppState,
    connection_id: ConnectionId,
    msg: TerminalMessage,
) -> anyhow::Result<()> {
    if state.debug_websocket {
        debug!("Handling terminal message: {}", msg.action);
    }
    
    match msg.action.as_str() {
        "create" => {
            let session_id = msg.session_id.unwrap_or_else(|| Uuid::new_v4().to_string());
            create_terminal_session(state, connection_id, session_id, msg.cols, msg.rows).await?;
        }
        "check" => {
            if let Some(session_id) = msg.session_id {
                check_terminal_session(state, connection_id, session_id).await?;
            }
        }
        "input" => {
            if let (Some(session_id), Some(input)) = (msg.session_id, msg.input) {
                send_input_to_terminal(state, session_id, input).await?;
            }
        }
        "resize" => {
            if let (Some(session_id), Some(cols), Some(rows)) = (msg.session_id, msg.cols, msg.rows) {
                resize_terminal(state, session_id, cols, rows).await?;
            }
        }
        "destroy" => {
            if let Some(session_id) = msg.session_id {
                destroy_terminal_session(state, session_id).await?;
            }
        }
        _ => {
            warn!("Unknown terminal action: {}", msg.action);
        }
    }
    
    Ok(())
}

// Terminal functions with full PTY integration
async fn create_terminal_session(
    state: &AppState,
    connection_id: ConnectionId,
    session_id: String,
    cols: Option<u16>,
    rows: Option<u16>,
) -> anyhow::Result<()> {
    // Check if session already exists
    {
        let sessions = state.terminal_sessions.read().await;
        if sessions.contains_key(&session_id) {
            // Update connection ID for existing session
            let mut sessions = state.terminal_sessions.write().await;
            if let Some(session) = sessions.get_mut(&session_id) {
                session.connection_id = connection_id;
            }
            send_terminal_response(state, &session_id, "created").await?;
            return Ok(());
        }
    }
    
    let cols = cols.unwrap_or(80);
    let rows = rows.unwrap_or(24);
    
    // Determine working directory and shell
    let cwd = state.exercises_path.clone();
    let shell = if cfg!(windows) {
        "powershell.exe"
    } else {
        "bash"
    };
    
    // Create PTY system
    let pty_system = native_pty_system();
    let pty_size = PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    };
    
    // Create PTY pair
    let pty_pair = pty_system.openpty(pty_size)?;
    
    // Spawn shell process
    let mut cmd = CommandBuilder::new(shell);
    cmd.cwd(&cwd);
    cmd.env("TERM", "xterm-color");
    
    let child = pty_pair.slave.spawn_command(cmd)?;
    
    // Get reader and writer
    let reader = pty_pair.master.try_clone_reader()?;
    let writer = pty_pair.master.take_writer()?;
    let master = pty_pair.master;
    
    // Create session
    let session = TerminalSession {
        session_id: session_id.clone(),
        connection_id,
    };
    
    let pty_handle = PtyHandle {
        writer: Arc::new(Mutex::new(writer)),
        child: Arc::new(Mutex::new(child)),
        master: Arc::new(Mutex::new(master)),
    };
    
    // Store session and handle
    {
        let mut sessions = state.terminal_sessions.write().await;
        sessions.insert(session_id.clone(), session);
    }
    
    {
        let mut handles = state.pty_handles.write().await;
        handles.insert(session_id.clone(), pty_handle);
    }
    
    // Spawn task to read PTY output and send to WebSocket
    let state_clone = state.clone();
    let session_id_clone = session_id.clone();
    tokio::spawn(async move {
        // Move reader to blocking thread for reading
        let (tx, mut rx) = tokio::sync::mpsc::channel(100);
        
        thread::spawn(move || {
            let mut reader = reader;
            let mut buffer = [0; 1024];
            
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) => break, // EOF
                    Ok(n) => {
                        let data = buffer[..n].to_vec();
                        if tx.blocking_send(data).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });
        
        // Handle data in async context
        while let Some(data) = rx.recv().await {
            let data_str = String::from_utf8_lossy(&data).to_string();
            
            let message = BroadcastMessage {
                msg_type: "terminal".to_string(),
                data: serde_json::json!({
                    "action": "output",
                    "sessionId": session_id_clone,
                    "data": data_str
                }),
            };
            
            let _ = state_clone.broadcast_tx.send(message);
        }
        
        // Send exit message when PTY closes
        let exit_message = BroadcastMessage {
            msg_type: "terminal".to_string(),
            data: serde_json::json!({
                "action": "exit",
                "sessionId": session_id_clone
            }),
        };
        
        let _ = state_clone.broadcast_tx.send(exit_message);
        
        // Clean up session
        {
            let mut sessions = state_clone.terminal_sessions.write().await;
            sessions.remove(&session_id_clone);
        }
        {
            let mut handles = state_clone.pty_handles.write().await;
            handles.remove(&session_id_clone);
        }
    });
    
    send_terminal_response(state, &session_id, "created").await?;
    
    if state.debug_websocket {
        info!("Terminal session {} created with PTY", session_id);
    }
    
    Ok(())
}

async fn check_terminal_session(
    state: &AppState,
    connection_id: ConnectionId,
    session_id: String,
) -> anyhow::Result<()> {
    let sessions = state.terminal_sessions.read().await;
    let handles = state.pty_handles.read().await;
    
    if let (Some(_session), Some(_handle)) = (sessions.get(&session_id), handles.get(&session_id)) {
        send_terminal_response(state, &session_id, "exists").await?;
        
        // Update connection ID for existing session
        drop(sessions);
        let mut sessions = state.terminal_sessions.write().await;
        if let Some(session) = sessions.get_mut(&session_id) {
            session.connection_id = connection_id;
        }
    } else {
        send_terminal_response(state, &session_id, "not_found").await?;
    }
    Ok(())
}

async fn send_input_to_terminal(
    state: &AppState,
    session_id: String,
    input: String,
) -> anyhow::Result<()> {
    let handles = state.pty_handles.read().await;
    if let Some(handle) = handles.get(&session_id) {
        let writer = handle.writer.clone();
        let input_clone = input.clone();
        
        // Move writing to blocking task since PTY writer is not async
        tokio::task::spawn_blocking(move || {
            if let Ok(mut writer) = writer.try_lock() {
                let _ = writer.write_all(input_clone.as_bytes());
                let _ = writer.flush();
            }
        }).await?;
        
        if state.debug_websocket {
            debug!("Sent input to terminal {}: {}", session_id, input);
        }
    } else {
        warn!("Terminal session {} not found for input", session_id);
    }
    Ok(())
}

async fn resize_terminal(
    state: &AppState,
    session_id: String,
    cols: u16,
    rows: u16,
) -> anyhow::Result<()> {
    let handles = state.pty_handles.read().await;
    if let Some(handle) = handles.get(&session_id) {
        let master = handle.master.clone();
        
        // Move resizing to blocking task since PTY operations are not async
        tokio::task::spawn_blocking(move || {
            if let Ok(mut master) = master.try_lock() {
                let new_size = PtySize {
                    rows,
                    cols,
                    pixel_width: 0,
                    pixel_height: 0,
                };
                let _ = master.resize(new_size);
            }
        }).await?;
        
        if state.debug_websocket {
            debug!("Resized terminal {} to {}x{}", session_id, cols, rows);
        }
    } else {
        warn!("Terminal session {} not found for resize", session_id);
    }
    Ok(())
}

async fn destroy_terminal_session(
    state: &AppState,
    session_id: String,
) -> anyhow::Result<()> {
    // Remove PTY handle and kill process
    {
        let mut handles = state.pty_handles.write().await;
        if let Some(handle) = handles.remove(&session_id) {
            let child = handle.child.clone();
            tokio::task::spawn_blocking(move || {
                if let Ok(mut child) = child.try_lock() {
                    let _ = child.kill();
                }
            });
        }
    }
    
    // Remove session
    {
        let mut sessions = state.terminal_sessions.write().await;
        sessions.remove(&session_id);
    }
    
    if state.debug_websocket {
        info!("Terminal session {} destroyed", session_id);
    }
    
    Ok(())
}

async fn cleanup_terminal_sessions(
    state: &AppState,
    connection_id: ConnectionId,
) -> anyhow::Result<()> {
    let mut sessions_to_remove = Vec::new();
    
    {
        let sessions = state.terminal_sessions.read().await;
        for (session_id, session) in sessions.iter() {
            if session.connection_id == connection_id {
                sessions_to_remove.push(session_id.clone());
            }
        }
    }
    
    // Clean up each session
    for session_id in sessions_to_remove {
        destroy_terminal_session(state, session_id).await?;
    }
    
    Ok(())
}

async fn send_terminal_response(
    state: &AppState,
    session_id: &str,
    action: &str,
) -> anyhow::Result<()> {
    let response = BroadcastMessage {
        msg_type: "terminal".to_string(),
        data: serde_json::json!({
            "action": action,
            "sessionId": session_id
        }),
    };
    
    let _ = state.broadcast_tx.send(response);
    Ok(())
}

// API handlers
async fn get_exercises(State(state): State<AppState>) -> Result<Json<Vec<ExerciseWithPath>>, StatusCode> {
    match scan_exercises(&state.exercises_path).await {
        Ok(exercises) => Ok(Json(exercises)),
        Err(e) => {
            error!("Error loading exercises: {}", e);
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

async fn get_exercise(
    AxumPath((chapter, exercise)): AxumPath<(String, String)>,
    State(state): State<AppState>,
) -> Result<Json<ExerciseDetails>, StatusCode> {
    let exercise_path = state.exercises_path.join(&chapter).join(&exercise);
    
    match load_exercise_details(&exercise_path, &format!("{}/{}", chapter, exercise)).await {
        Ok(details) => Ok(Json(details)),
        Err(e) => {
            error!("Error loading exercise {}/{}: {}", chapter, exercise, e);
            Err(StatusCode::NOT_FOUND)
        }
    }
}

async fn save_exercise_code(
    AxumPath((chapter, exercise)): AxumPath<(String, String)>,
    State(state): State<AppState>,
    Json(request): Json<SaveCodeRequest>,
) -> Result<Json<ApiResponse<()>>, StatusCode> {
    let exercise_path = state.exercises_path.join(&chapter).join(&exercise);
    let main_path = exercise_path.join("src").join("main.rs");
    
    match fs::write(&main_path, &request.code).await {
        Ok(_) => {
            // Broadcast file change
            let exercise_name = match load_exercise_title(&exercise_path).await {
                Ok(title) => title,
                Err(_) => format!("{}/{}", chapter, exercise),
            };
            
            let broadcast_msg = BroadcastMessage {
                msg_type: "file_updated".to_string(),
                data: serde_json::json!({
                    "exercise": exercise_name,
                    "file": "src/main.rs"
                }),
            };
            
            let _ = state.broadcast_tx.send(broadcast_msg);
            
            Ok(Json(ApiResponse::success(())))
        }
        Err(e) => {
            error!("Error saving code for {}/{}: {}", chapter, exercise, e);
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

async fn test_exercise(
    AxumPath((chapter, exercise)): AxumPath<(String, String)>,
    State(state): State<AppState>,
) -> Result<Json<CargoResult>, StatusCode> {
    let exercise_path = state.exercises_path.join(&chapter).join(&exercise);
    
    match run_cargo_command("test", &exercise_path, vec!["--", "--nocapture"]).await {
        Ok(result) => Ok(Json(result)),
        Err(e) => {
            error!("Error running tests for {}/{}: {}", chapter, exercise, e);
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

async fn run_exercise(
    AxumPath((chapter, exercise)): AxumPath<(String, String)>,
    State(state): State<AppState>,
) -> Result<Json<CargoResult>, StatusCode> {
    let exercise_path = state.exercises_path.join(&chapter).join(&exercise);
    
    match run_cargo_command("run", &exercise_path, vec![]).await {
        Ok(result) => Ok(Json(result)),
        Err(e) => {
            error!("Error running exercise {}/{}: {}", chapter, exercise, e);
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

async fn check_exercise(
    AxumPath((chapter, exercise)): AxumPath<(String, String)>,
    State(state): State<AppState>,
) -> Result<Json<CargoResult>, StatusCode> {
    let exercise_path = state.exercises_path.join(&chapter).join(&exercise);
    
    match run_cargo_command("clippy", &exercise_path, vec!["--", "-W", "clippy::all"]).await {
        Ok(result) => Ok(Json(result)),
        Err(e) => {
            error!("Error running clippy for {}/{}: {}", chapter, exercise, e);
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

async fn get_progress(State(state): State<AppState>) -> Result<Json<ProgressData>, StatusCode> {
    match ensure_progress_file(&state.progress_path, &state.exercises_path).await {
        Ok(progress) => Ok(Json(progress)),
        Err(e) => {
            error!("Error loading progress: {}", e);
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

async fn complete_exercise(
    State(state): State<AppState>,
    Json(request): Json<CompleteExerciseRequest>,
) -> Result<Json<ApiResponse<ProgressData>>, StatusCode> {
    match update_exercise_completion(&state.progress_path, &state.exercises_path, &request).await {
        Ok(progress) => Ok(Json(ApiResponse::success_with_extra(
            progress,
            serde_json::json!({"message": "Exercise completed successfully"})
        ))),
        Err(e) => {
            error!("Error updating progress: {}", e);
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

async fn track_hint_usage(
    State(state): State<AppState>,
    Json(request): Json<HintRequest>,
) -> Result<Json<ApiResponse<ProgressData>>, StatusCode> {
    match update_hint_usage(&state.progress_path, &state.exercises_path, &request).await {
        Ok(progress) => Ok(Json(ApiResponse::success(progress))),
        Err(e) => {
            error!("Error tracking hint usage: {}", e);
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

async fn track_exercise_view(
    State(state): State<AppState>,
    Json(request): Json<ViewRequest>,
) -> Result<Json<ApiResponse<ProgressData>>, StatusCode> {
    match update_exercise_view(&state.progress_path, &state.exercises_path, &request).await {
        Ok(progress) => Ok(Json(ApiResponse::success(progress))),
        Err(e) => {
            error!("Error tracking exercise view: {}", e);
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

async fn get_book_chapter(
    AxumPath(chapter): AxumPath<String>,
) -> Result<Json<BookResponse>, StatusCode> {
    let book_url = format!("https://doc.rust-lang.org/book/ch{}.html", chapter);
    Ok(Json(BookResponse {
        url: book_url,
        chapter,
    }))
}

// Static file handlers
#[cfg(feature = "embed-assets")]
async fn serve_static_files(uri: axum::http::Uri) -> Result<Response, StatusCode> {
    let path = uri.path().trim_start_matches('/');
    
    // Try to serve the requested file
    if let Some(content) = Assets::get(path) {
        let mime = mime_guess::from_path(path).first_or_octet_stream();
        return Ok((
            [(header::CONTENT_TYPE, HeaderValue::from_str(mime.as_ref()).unwrap())],
            content.data,
        ).into_response());
    }
    
    // Fallback to index.html for client-side routing
    if let Some(content) = Assets::get("index.html") {
        return Ok((
            [(header::CONTENT_TYPE, HeaderValue::from_static("text/html"))],
            content.data,
        ).into_response());
    }
    
    Err(StatusCode::NOT_FOUND)
}

#[cfg(not(feature = "embed-assets"))]
async fn serve_static_files(uri: axum::http::Uri) -> Result<Response, StatusCode> {
    use tokio::fs;
    use std::path::Path;
    
    let path = uri.path().trim_start_matches('/');
    let web_dist = Path::new("web/dist");
    
    // Security check: prevent directory traversal
    if path.contains("..") {
        return Err(StatusCode::BAD_REQUEST);
    }
    
    let file_path = if path.is_empty() { 
        web_dist.join("index.html") 
    } else { 
        web_dist.join(path) 
    };
    
    // Try to serve the requested file
    if file_path.exists() && file_path.is_file() {
        match fs::read(&file_path).await {
            Ok(content) => {
                let mime = mime_guess::from_path(&file_path).first_or_octet_stream();
                return Ok((
                    [(header::CONTENT_TYPE, HeaderValue::from_str(mime.as_ref()).unwrap())],
                    content,
                ).into_response());
            }
            Err(_) => return Err(StatusCode::INTERNAL_SERVER_ERROR),
        }
    }
    
    // Fallback to index.html for client-side routing
    let index_path = web_dist.join("index.html");
    if index_path.exists() {
        match fs::read(&index_path).await {
            Ok(content) => {
                return Ok((
                    [(header::CONTENT_TYPE, HeaderValue::from_static("text/html"))],
                    content,
                ).into_response());
            }
            Err(_) => return Err(StatusCode::INTERNAL_SERVER_ERROR),
        }
    }
    
    Err(StatusCode::NOT_FOUND)
}

#[cfg(feature = "embed-assets")]
async fn serve_monaco_files(
    AxumPath(path): AxumPath<String>,
) -> Result<Response, StatusCode> {
    if let Some(content) = MonacoAssets::get(&path) {
        let mime = mime_guess::from_path(&path).first_or_octet_stream();
        Ok((
            [(header::CONTENT_TYPE, HeaderValue::from_str(mime.as_ref()).unwrap())],
            content.data,
        ).into_response())
    } else {
        Err(StatusCode::NOT_FOUND)
    }
}

#[cfg(not(feature = "embed-assets"))]
async fn serve_monaco_files(
    AxumPath(path): AxumPath<String>,
) -> Result<Response, StatusCode> {
    use tokio::fs;
    use std::path::Path;
    
    // Security check: prevent directory traversal
    if path.contains("..") {
        return Err(StatusCode::BAD_REQUEST);
    }
    
    let file_path = Path::new("web/node_modules/monaco-editor").join(&path);
    
    if file_path.exists() && file_path.is_file() {
        match fs::read(&file_path).await {
            Ok(content) => {
                let mime = mime_guess::from_path(&file_path).first_or_octet_stream();
                Ok((
                    [(header::CONTENT_TYPE, HeaderValue::from_str(mime.as_ref()).unwrap())],
                    content,
                ).into_response())
            }
            Err(_) => Err(StatusCode::INTERNAL_SERVER_ERROR),
        }
    } else {
        Err(StatusCode::NOT_FOUND)
    }
}

// Helper functions
async fn scan_exercises(exercises_path: &std::path::Path) -> anyhow::Result<Vec<ExerciseWithPath>> {
    let mut exercises = Vec::new();
    
    if !exercises_path.exists() {
        return Ok(exercises);
    }
    
    for chapter_entry in WalkDir::new(exercises_path).max_depth(1) {
        let chapter_entry = chapter_entry?;
        if !chapter_entry.file_type().is_dir() {
            continue;
        }
        
        let chapter_name = chapter_entry.file_name().to_string_lossy();
        if !chapter_name.starts_with("ch") {
            continue;
        }
        
        for exercise_entry in WalkDir::new(chapter_entry.path()).max_depth(1) {
            let exercise_entry = exercise_entry?;
            if !exercise_entry.file_type().is_dir() {
                continue;
            }
            
            let exercise_name = exercise_entry.file_name().to_string_lossy();
            if !exercise_name.starts_with("ex") {
                continue;
            }
            
            let metadata_path = exercise_entry.path().join("metadata.json");
            if metadata_path.exists() {
                match fs::read_to_string(&metadata_path).await {
                    Ok(content) => {
                        match serde_json::from_str::<ExerciseMetadata>(&content) {
                            Ok(metadata) => {
                                exercises.push(ExerciseWithPath {
                                    metadata,
                                    path: format!("{}/{}", chapter_name, exercise_name),
                                });
                            }
                            Err(e) => {
                                warn!("Error parsing metadata for {}/{}: {}", chapter_name, exercise_name, e);
                            }
                        }
                    }
                    Err(e) => {
                        warn!("Error reading metadata for {}/{}: {}", chapter_name, exercise_name, e);
                    }
                }
            }
        }
    }
    
    // Sort by chapter and exercise number
    exercises.sort_by(|a, b| {
        match a.metadata.chapter.cmp(&b.metadata.chapter) {
            std::cmp::Ordering::Equal => a.metadata.exercise_number.cmp(&b.metadata.exercise_number),
            other => other,
        }
    });
    
    Ok(exercises)
}

async fn load_exercise_details(
    exercise_path: &std::path::Path,
    path: &str,
) -> anyhow::Result<ExerciseDetails> {
    // Load metadata
    let metadata_path = exercise_path.join("metadata.json");
    let metadata_content = fs::read_to_string(&metadata_path).await?;
    let metadata: ExerciseMetadata = serde_json::from_str(&metadata_content)?;
    
    // Load main source file
    let main_path = exercise_path.join("src").join("main.rs");
    let main_content = fs::read_to_string(&main_path).await?;
    
    // Load README
    let readme_path = exercise_path.join("README.md");
    let readme = fs::read_to_string(&readme_path).await?;
    
    // Load hints if available
    let hints_path = exercise_path.join("hints.md");
    let hints = if hints_path.exists() {
        fs::read_to_string(&hints_path).await.unwrap_or_default()
    } else {
        String::new()
    };
    
    Ok(ExerciseDetails {
        metadata,
        main_content,
        readme,
        hints,
        path: path.to_string(),
    })
}

async fn load_exercise_title(exercise_path: &std::path::Path) -> anyhow::Result<String> {
    let metadata_path = exercise_path.join("metadata.json");
    let metadata_content = fs::read_to_string(&metadata_path).await?;
    let metadata: ExerciseMetadata = serde_json::from_str(&metadata_content)?;
    Ok(metadata.title)
}

async fn run_cargo_command(
    command: &str,
    cwd: &std::path::Path,
    args: Vec<&str>,
) -> anyhow::Result<CargoResult> {
    let mut cmd = Command::new("cargo");
    cmd.arg(command)
        .args(&args)
        .current_dir(cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    
    let output = timeout(Duration::from_secs(60), cmd.output()).await??;
    
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let combined_output = format!("{}{}", stdout, stderr);
    
    Ok(CargoResult {
        success: output.status.success(),
        code: output.status.code(),
        stdout,
        stderr,
        output: combined_output,
    })
}

async fn count_total_exercises(exercises_path: &std::path::Path) -> anyhow::Result<u32> {
    let mut count = 0;
    
    if !exercises_path.exists() {
        return Ok(50); // Fallback
    }
    
    for chapter_entry in WalkDir::new(exercises_path).max_depth(1) {
        let chapter_entry = chapter_entry?;
        if !chapter_entry.file_type().is_dir() {
            continue;
        }
        
        let chapter_name = chapter_entry.file_name().to_string_lossy();
        if !chapter_name.starts_with("ch") {
            continue;
        }
        
        for exercise_entry in WalkDir::new(chapter_entry.path()).max_depth(1) {
            let exercise_entry = exercise_entry?;
            if !exercise_entry.file_type().is_dir() {
                continue;
            }
            
            let exercise_name = exercise_entry.file_name().to_string_lossy();
            if exercise_name.starts_with("ex") {
                count += 1;
            }
        }
    }
    
    Ok(if count > 0 { count } else { 50 })
}

async fn ensure_progress_file(
    progress_path: &std::path::Path,
    exercises_path: &std::path::Path,
) -> anyhow::Result<ProgressData> {
    // Ensure the progress directory exists
    if let Some(parent) = progress_path.parent() {
        fs::create_dir_all(parent).await?;
    }
    
    let total_exercises = count_total_exercises(exercises_path).await?;
    
    if !progress_path.exists() {
        info!("Creating new progress file: {:?}", progress_path);
        info!("Detected {} total exercises", total_exercises);
        
        let default_progress = ProgressData {
            user_id: "default".to_string(),
            created_at: Utc::now().to_rfc3339(),
            overall_progress: 0.0,
            chapters_completed: 0,
            exercises_completed: 0,
            total_exercises,
            current_streak: 0,
            longest_streak: 0,
            total_time_minutes: 0,
            chapters: serde_json::Value::Object(serde_json::Map::new()),
            exercise_history: Vec::new(),
            achievements: Vec::new(),
            session_stats: SessionStats {
                exercises_viewed: 0,
                exercises_completed: 0,
                hints_used: 0,
                time_spent: 0,
            },
        };
        
        let content = serde_json::to_string_pretty(&default_progress)?;
        fs::write(progress_path, content).await?;
        info!("Progress file created successfully");
        return Ok(default_progress);
    }
    
    // File exists, load and validate it
    let content = fs::read_to_string(progress_path).await?;
    let mut progress: ProgressData = serde_json::from_str(&content)?;
    
    // Ensure all required properties exist (for backwards compatibility)
    if progress.session_stats.exercises_viewed == 0 && progress.session_stats.exercises_completed == 0 && progress.session_stats.hints_used == 0 && progress.session_stats.time_spent == 0 {
        // Initialize default session stats if they're all zero (likely missing)
        progress.session_stats = SessionStats {
            exercises_viewed: 0,
            exercises_completed: 0,
            hints_used: 0,
            time_spent: 0,
        };
    }
    
    // Update total exercises count if it's wrong or missing
    if progress.total_exercises == 0 || progress.total_exercises == 200 {
        progress.total_exercises = total_exercises;
        let content = serde_json::to_string_pretty(&progress)?;
        fs::write(progress_path, content).await?;
        info!("Updated total exercises count to {}", total_exercises);
    }
    
    Ok(progress)
}

async fn update_exercise_completion(
    progress_path: &std::path::Path,
    exercises_path: &std::path::Path,
    request: &CompleteExerciseRequest,
) -> anyhow::Result<ProgressData> {
    let mut progress = ensure_progress_file(progress_path, exercises_path).await?;
    
    // Check if already completed to avoid duplicates
    let existing_entry = progress.exercise_history.iter().find(|entry| entry.exercise_id == request.exercise_id);
    let is_already_completed = existing_entry.map_or(false, |entry| entry.completed_at.is_some());
    
    info!("Checking completion for {}: already completed: {}", request.exercise_id, is_already_completed);
    
    if is_already_completed {
        info!("Exercise {} already completed", request.exercise_id);
        return Ok(progress);
    }
    
    // Update progress
    progress.exercises_completed += 1;
    progress.session_stats.exercises_completed += 1;
    progress.total_time_minutes += request.time_taken_minutes.unwrap_or(0);
    progress.overall_progress = progress.exercises_completed as f64 / progress.total_exercises as f64;
    
    // Update or add to exercise history
    if let Some(entry) = progress.exercise_history.iter_mut().find(|entry| entry.exercise_id == request.exercise_id) {
        entry.completed_at = Some(Utc::now().to_rfc3339());
        entry.time_taken_minutes = request.time_taken_minutes;
        entry.status = "completed".to_string();
        entry.session_id = Some(format!("session_{}", Utc::now().timestamp_millis()));
    } else {
        progress.exercise_history.push(ExerciseHistoryEntry {
            exercise_id: request.exercise_id.clone(),
            viewed_at: None,
            completed_at: Some(Utc::now().to_rfc3339()),
            time_taken_minutes: request.time_taken_minutes,
            status: "completed".to_string(),
            session_id: Some(format!("session_{}", Utc::now().timestamp_millis())),
            hints_used: None,
        });
    }
    
    // Save updated progress
    let content = serde_json::to_string_pretty(&progress)?;
    fs::write(progress_path, content).await?;
    
    info!(
        "Exercise completed: {} in {} minutes",
        request.exercise_id,
        request.time_taken_minutes.unwrap_or(0)
    );
    info!(
        "Total exercises completed: {}/{}",
        progress.exercises_completed,
        progress.total_exercises
    );
    
    Ok(progress)
}

async fn update_hint_usage(
    progress_path: &std::path::Path,
    exercises_path: &std::path::Path,
    request: &HintRequest,
) -> anyhow::Result<ProgressData> {
    let mut progress = ensure_progress_file(progress_path, exercises_path).await?;
    
    // Update hint usage stats
    progress.session_stats.hints_used += 1;
    
    // Add to exercise history if not already tracked for this exercise
    if let Some(entry) = progress.exercise_history.iter_mut().find(|entry| entry.exercise_id == request.exercise_id) {
        if let Some(ref mut hints_used) = entry.hints_used {
            if !hints_used.contains(&request.hint_level) {
                hints_used.push(request.hint_level);
            }
        } else {
            entry.hints_used = Some(vec![request.hint_level]);
        }
    } else {
        progress.exercise_history.push(ExerciseHistoryEntry {
            exercise_id: request.exercise_id.clone(),
            viewed_at: Some(Utc::now().to_rfc3339()),
            completed_at: None,
            time_taken_minutes: None,
            status: "in_progress".to_string(),
            session_id: None,
            hints_used: Some(vec![request.hint_level]),
        });
    }
    
    // Save updated progress
    let content = serde_json::to_string_pretty(&progress)?;
    fs::write(progress_path, content).await?;
    
    info!("Hint used: {}, level {}", request.exercise_id, request.hint_level);
    Ok(progress)
}

async fn update_exercise_view(
    progress_path: &std::path::Path,
    exercises_path: &std::path::Path,
    request: &ViewRequest,
) -> anyhow::Result<ProgressData> {
    let mut progress = ensure_progress_file(progress_path, exercises_path).await?;
    
    // Update view stats
    progress.session_stats.exercises_viewed += 1;
    
    // Check if already viewed
    if !progress.exercise_history.iter().any(|entry| entry.exercise_id == request.exercise_id) {
        progress.exercise_history.push(ExerciseHistoryEntry {
            exercise_id: request.exercise_id.clone(),
            viewed_at: Some(Utc::now().to_rfc3339()),
            completed_at: None,
            time_taken_minutes: None,
            status: "viewed".to_string(),
            session_id: None,
            hints_used: None,
        });
    }
    
    // Save updated progress
    let content = serde_json::to_string_pretty(&progress)?;
    fs::write(progress_path, content).await?;
    
    info!("Exercise viewed: {}", request.exercise_id);
    Ok(progress)
}

async fn initialize_progress_system(state: &AppState) -> anyhow::Result<()> {
    match ensure_progress_file(&state.progress_path, &state.exercises_path).await {
        Ok(_) => {
            info!("📊 Progress system initialized");
            Ok(())
        }
        Err(e) => {
            error!("Failed to initialize progress system: {}", e);
            Err(e)
        }
    }
}

async fn setup_file_watcher(state: AppState) -> anyhow::Result<()> {
    let exercises_path = state.exercises_path.clone();
    let broadcast_tx = state.broadcast_tx.clone();
    
    tokio::spawn(async move {
        let (tx, mut rx) = tokio::sync::mpsc::channel(100);
        
        let mut watcher = match RecommendedWatcher::new(
            move |res| {
                if let Err(e) = tx.blocking_send(res) {
                    error!("Failed to send file watcher event: {}", e);
                }
            },
            notify::Config::default(),
        ) {
            Ok(watcher) => watcher,
            Err(e) => {
                error!("Failed to create file watcher: {}", e);
                return;
            }
        };
        
        if let Err(e) = watcher.watch(&exercises_path, RecursiveMode::Recursive) {
            error!("Failed to watch exercises directory: {}", e);
            return;
        }
        
        while let Some(res) = rx.recv().await {
            match res {
                Ok(event) => {
                    for path in event.paths {
                        if let Ok(relative_path) = path.strip_prefix(&exercises_path) {
                            let path_parts: Vec<_> = relative_path.components().collect();
                            if path_parts.len() >= 2 {
                                let chapter_dir = path_parts[0].as_os_str().to_string_lossy();
                                let exercise_dir = path_parts[1].as_os_str().to_string_lossy();
                                
                                // Try to load exercise metadata to get title
                                let metadata_path = exercises_path.join(&*chapter_dir).join(&*exercise_dir).join("metadata.json");
                                let exercise_name = if metadata_path.exists() {
                                    match load_exercise_title(&exercises_path.join(&*chapter_dir).join(&*exercise_dir)).await {
                                        Ok(title) => title,
                                        Err(_) => exercise_dir.replace('_', " ").replacen("ex", "", 1),
                                    }
                                } else {
                                    exercise_dir.replace('_', " ").replacen("ex", "", 1)
                                };
                                
                                let broadcast_msg = BroadcastMessage {
                                    msg_type: "file_changed".to_string(),
                                    data: serde_json::json!({
                                        "exercise": exercise_name,
                                        "file": relative_path.to_string_lossy()
                                    }),
                                };
                                
                                let _ = broadcast_tx.send(broadcast_msg);
                            }
                        }
                    }
                }
                Err(e) => {
                    error!("File watcher error: {}", e);
                }
            }
        }
    });
    
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install signal handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {
            info!("Shutting down gracefully...");
        },
        _ = terminate => {
            info!("Shutting down gracefully...");
        },
    }
}