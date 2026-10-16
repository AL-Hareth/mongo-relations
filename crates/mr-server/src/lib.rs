//! The mongo-relations server: one binary, two listeners (D11) over one
//! [`mr_engine::Engine`].

pub mod auth;
pub mod config;
pub mod errors;
pub mod grpc;
pub mod http;

use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Context;
use mr_engine::Engine;
use mr_proto::pb::admin_service_server::AdminServiceServer;
use mr_proto::pb::relations_service_server::RelationsServiceServer;
use tokio::net::TcpListener;

use crate::auth::Auth;
use crate::config::Config;

/// State shared by both front-ends.
#[derive(Clone)]
pub struct AppState {
    pub engine: Engine,
    pub auth: Arc<Auth>,
    pub max_body_bytes: usize,
    pub max_grpc_message_bytes: usize,
}

impl AppState {
    pub fn new(engine: Engine, config: &Config) -> Self {
        Self {
            engine,
            auth: Arc::new(Auth::new(&config.auth)),
            max_body_bytes: config.server.max_body_bytes,
            max_grpc_message_bytes: config.server.max_grpc_message_bytes,
        }
    }
}

/// Bound listeners, ready to serve.
pub struct Server {
    state: AppState,
    grpc: TcpListener,
    http: TcpListener,
}

impl Server {
    pub async fn bind(
        state: AppState,
        grpc_addr: SocketAddr,
        http_addr: SocketAddr,
    ) -> anyhow::Result<Self> {
        let grpc = TcpListener::bind(grpc_addr)
            .await
            .with_context(|| format!("binding gRPC on {grpc_addr}"))?;
        let http = TcpListener::bind(http_addr)
            .await
            .with_context(|| format!("binding HTTP on {http_addr}"))?;
        Ok(Self { state, grpc, http })
    }

    pub fn grpc_addr(&self) -> SocketAddr {
        self.grpc.local_addr().expect("bound")
    }

    pub fn http_addr(&self) -> SocketAddr {
        self.http.local_addr().expect("bound")
    }

    /// Serve until `shutdown` resolves.
    pub async fn serve(
        self,
        shutdown: impl Future<Output = ()> + Send + 'static,
    ) -> anyhow::Result<()> {
        let (tx, rx) = tokio::sync::watch::channel(false);
        let wait = |mut rx: tokio::sync::watch::Receiver<bool>| async move {
            let _ = rx.wait_for(|v| *v).await;
        };
        let svc = grpc::Grpc {
            state: self.state.clone(),
        };
        let grpc = tonic::transport::Server::builder()
            .add_service(RelationsServiceServer::new(svc.clone()))
            .add_service(AdminServiceServer::new(svc))
            .serve_with_incoming_shutdown(
                tokio_stream::wrappers::TcpListenerStream::new(self.grpc),
                wait(rx.clone()),
            );
        let http =
            axum::serve(self.http, http::router(self.state)).with_graceful_shutdown(wait(rx));
        tokio::spawn(async move {
            shutdown.await;
            let _ = tx.send(true);
        });
        let (g, h) = tokio::join!(grpc, http);
        g.context("gRPC server")?;
        h.context("HTTP server")?;
        Ok(())
    }
}
