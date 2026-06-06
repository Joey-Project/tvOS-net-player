pub mod config;
pub mod generated;
pub mod grpc_services;
pub mod library;
pub mod media;
pub mod playback;
pub mod task_registry;

use std::{net::SocketAddr, sync::Arc};

use axum::{Router, routing::get};
use generated::tvos_net_player::v1::{
    cache_service_server::CacheServiceServer, library_service_server::LibraryServiceServer,
    server_service_server::ServerServiceServer, task_service_server::TaskServiceServer,
};
use tokio::net::TcpListener;
use tonic::transport::Server;

use crate::{
    config::CacheServerOptions,
    grpc_services::{CacheGrpcService, LibraryGrpcService, ServerGrpcService, TaskGrpcService},
    library::LocalMediaLibrary,
    media::{MediaState, media_get, media_head},
    playback::PlaybackUriFactory,
    task_registry::BilibiliTaskRegistry,
};

#[derive(Clone)]
pub struct AppState {
    pub options: Arc<CacheServerOptions>,
    pub library: Arc<LocalMediaLibrary>,
    pub playback_uri_factory: Arc<PlaybackUriFactory>,
    pub tasks: Arc<BilibiliTaskRegistry>,
}

impl AppState {
    pub fn new(options: CacheServerOptions) -> Self {
        options.validate().expect("invalid cache server options");
        let options = Arc::new(options);
        let library = Arc::new(LocalMediaLibrary::new(Arc::clone(&options)));
        let playback_uri_factory = Arc::new(PlaybackUriFactory::new(Arc::clone(&options)));
        let tasks = Arc::new(BilibiliTaskRegistry::default());

        Self {
            options,
            library,
            playback_uri_factory,
            tasks,
        }
    }
}

pub async fn run(
    options: CacheServerOptions,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let state = AppState::new(options);
    run_with_state(state).await
}

pub async fn run_with_state(
    state: AppState,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let grpc_addr = state.options.grpc_listen_addr()?;
    let media_addr = state.options.media_listen_addr()?;
    let grpc_state = state.clone();
    let media_state = state.clone();

    let grpc_server = run_grpc_server(grpc_addr, grpc_state);
    let media_server = run_media_server(media_addr, media_state);

    tokio::select! {
        result = grpc_server => result,
        result = media_server => result,
        _ = shutdown_signal() => Ok(()),
    }
}

pub async fn run_grpc_server(
    addr: SocketAddr,
    state: AppState,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    Server::builder()
        .add_service(ServerServiceServer::new(ServerGrpcService::new(
            state.clone(),
        )))
        .add_service(LibraryServiceServer::new(LibraryGrpcService::new(
            state.clone(),
        )))
        .add_service(TaskServiceServer::new(TaskGrpcService::new(state.clone())))
        .add_service(CacheServiceServer::new(CacheGrpcService::new(state)))
        .serve(addr)
        .await?;
    Ok(())
}

pub async fn run_media_server(
    addr: SocketAddr,
    state: AppState,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let router = Router::new()
        .route("/", get(root))
        .route(
            "/media/{item_id}/{variant_id}",
            get(media_get).head(media_head),
        )
        .with_state(MediaState::new(state));

    let listener = TcpListener::bind(addr).await?;
    axum::serve(listener, router).await?;
    Ok(())
}

async fn root() -> axum::Json<serde_json::Value> {
    axum::Json(serde_json::json!({
        "service": "TVOSNetPlayer.CacheServer",
        "controlPlane": "gRPC",
        "mediaPlane": "HTTP"
    }))
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}
