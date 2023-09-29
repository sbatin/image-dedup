mod analyzer;
mod manager;
mod cache;
mod disjoint_set;

use analyzer::{Analyzer, AnalyzeRequest, Groups, FileInfo};
use manager::{TaskManager, TaskResponse};
use std::{
    path::PathBuf,
    sync::Arc, time::Instant,
};
use serde::{Serialize, Deserialize};
use eyre::{Result, Report};
use axum::{
    http::{Request, StatusCode},
    extract::{Query, State},
    routing::{get, get_service, post},
    response::{
        Json, IntoResponse, Response,
        sse::{Event, KeepAlive, Sse},
    },
    Router,
};
use tower::ServiceExt;
use tower_http::{
    services,
    trace::TraceLayer,
};
use tokio::{
    task::JoinHandle,
    sync::{mpsc, oneshot, watch},
};
use futures::stream::{Stream, StreamExt};
use tokio_stream::wrappers::WatchStream;
use uuid::Uuid;

type TaskResult = Result<Groups>;

enum AnalyzeCommand {
    Submit(AnalyzeRequest, oneshot::Sender<Uuid>),
    Subscribe(Uuid, oneshot::Sender<Option<watch::Receiver<usize>>>),
    Poll(Uuid, oneshot::Sender<Option<TaskResponse<usize, TaskResult>>>),
}

async fn task_analyzer(mut rx: mpsc::Receiver<AnalyzeCommand>) {
    tracing::info!("manager task started");

    let engine = Arc::new(Analyzer::new());
    let mut manager: TaskManager<Uuid, usize, TaskResult> = TaskManager::new();

    while let Some(command) = rx.recv().await {
        match command {
            AnalyzeCommand::Submit(req, tx) => {
                tracing::info!("analyze task {:?} submitted", req);
                let engine = engine.clone();
                let task_id = Uuid::new_v4();
                manager.submit(task_id, move |tx| {
                    let started = Instant::now();
                    let result = engine.analyze(&req, tx);
                    let elapsed = started.elapsed();
                    tracing::info!("analyze task {:?} completed in {:?}", req, elapsed);
                    result
                });
                if let Err(_) = tx.send(task_id) {
                    tracing::error!("unable to send response back to the client");
                }
            }
            AnalyzeCommand::Subscribe(task_id, tx) => {
                let rx = manager.progress(&task_id);
                if let Err(_) = tx.send(rx) {
                    tracing::error!("unable to send response back to the client");
                }
            }
            AnalyzeCommand::Poll(task_id, tx) => {
                let resp = manager.poll(&task_id).await;
                if let Err(_) = tx.send(resp) {
                    tracing::error!("unable to send response back to the client");
                }
            }
        }
    }

    tracing::info!("manager task exiting");
}

fn spawn_analyzer() -> (JoinHandle<()>, mpsc::Sender<AnalyzeCommand>) {
    let (tx, rx) = mpsc::channel(32);
    let join_handle = tokio::spawn(task_analyzer(rx));
    (join_handle, tx)
}

enum AppError {
    Internal(Report),
    Provided(StatusCode),
}

impl AppError {
    fn not_found() -> Self {
        Self::Provided(StatusCode::NOT_FOUND)
    }
}

impl<T> From<T> for AppError
where
    T: Into<Report>
{
    fn from(inner: T) -> Self {
        Self::Internal(inner.into())
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let status_code = match self {
            Self::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
            Self::Provided(code) => code,
        };
        status_code.into_response()
    }
}

type JsonResponse<T> = Result<Json<T>, AppError>;

#[derive(Clone)]
struct AppState {
    task_sender: mpsc::Sender<AnalyzeCommand>,
}

#[derive(Serialize)]
#[serde(tag = "type")]
enum AnalyzeResponse {
    Pending { progress: usize },
    Completed { data: Groups },
    Failed { error: String },
}

#[derive(Serialize, Deserialize)]
struct PathParams {
    path: PathBuf,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TaskParams {
    task_id: Uuid,
}

async fn list_folder(Query(params): Query<PathParams>) -> JsonResponse<Vec<FileInfo>> {
    let files = analyzer::list_dir(&params.path)?;
    Ok(Json(files))
}

async fn analyze(
    State(state): State<AppState>,
    Query(req): Query<AnalyzeRequest>,
) -> JsonResponse<TaskParams> {
    let (tx, rx) = oneshot::channel();

    state
        .task_sender
        .send(AnalyzeCommand::Submit(req, tx))
        .await?;

    let task_id = rx.await?;

    Ok(Json(TaskParams { task_id }))
}

async fn poll(
    State(state): State<AppState>,
    Query(params): Query<TaskParams>,
) -> JsonResponse<AnalyzeResponse> {
    let (tx, rx) = oneshot::channel();

    state
        .task_sender
        .send(AnalyzeCommand::Poll(params.task_id, tx))
        .await?;

    let resp = rx.await?;
    let resp = resp.ok_or_else(|| AppError::not_found())?;
    Ok(Json(match resp {
        TaskResponse::Pending(progress) => AnalyzeResponse::Pending { progress },
        TaskResponse::Completed(Ok(data)) => AnalyzeResponse::Completed { data },
        TaskResponse::Completed(Err(err)) => AnalyzeResponse::Failed { error: err.to_string() }
    }))
}

async fn subscribe(
    State(state): State<AppState>,
    Query(params): Query<TaskParams>,
) -> Result<Sse<impl Stream<Item = serde_json::error::Result<Event>>>, AppError> {
    tracing::info!("SSE handler called {:?}", params.task_id);

    let (tx, rx) = oneshot::channel();

    state
        .task_sender
        .send(AnalyzeCommand::Subscribe(params.task_id, tx))
        .await?;

    let resp = rx.await?;
    let resp = resp.ok_or_else(|| AppError::not_found())?;
    let stream = WatchStream::new(resp).map(|p| Event::default().json_data(p));
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().init();
    tracing::info!("starting...");

    let (_, task_sender) = spawn_analyzer();
    let shared_state = AppState { task_sender };

    let app = Router::new()
        .route("/", get_service(services::ServeFile::new("client/dist/index.html")))
        .route("/image", get(|request: Request<_>| {
            // TODO: handle errors here
            let params: Query<PathParams> = Query::try_from_uri(request.uri()).unwrap();
            let service = services::ServeFile::new(&params.path);
            service.oneshot(request)
        }))
        .route("/list_folder", get(list_folder))
        .route("/analyze", post(analyze))
        .route("/poll", get(poll))
        .route("/subscribe", get(subscribe))
        .nest_service("/static", services::ServeDir::new("client/dist"))
        .nest_service("/assets", services::ServeDir::new("client/dist/assets"))
        .with_state(shared_state)
        .layer(TraceLayer::new_for_http());

    axum::Server::bind(&"0.0.0.0:3000".parse()?)
        .serve(app.into_make_service())
        .await?;

    tracing::info!("done");
    Ok(())
}
