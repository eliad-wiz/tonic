use super::{grpc_timeout::GrpcTimeout, reconnect::Reconnect, AddOrigin, UserAgent};
use crate::transport::{BoxFuture, Endpoint};
use http::Uri;
use hyper::rt;
use hyper::{client::conn::http2::Builder, rt::Executor};
use std::{
    fmt,
    task::{Context, Poll},
};
use tower::load::Load;
use tower::{
    layer::Layer,
    limit::{concurrency::ConcurrencyLimitLayer, rate::RateLimitLayer},
    util::BoxService,
    ServiceBuilder, ServiceExt,
};
use tower_service::Service;

pub(crate) type Request = axum::extract::Request;
pub(crate) type Response = axum::response::Response;
pub(crate) struct Connection {
    inner: BoxService<Request, Response, crate::Error>,
}

impl Connection {
    fn new<C>(connector: C, endpoint: Endpoint, is_lazy: bool) -> Self
    where
        C: Service<Uri> + Send + 'static,
        C::Error: Into<crate::Error> + Send,
        C::Future: Unpin + Send,
        C::Response: rt::Read + rt::Write + Unpin + Send + 'static,
    {
        let mut settings: Builder<super::SharedExec> = Builder::new(endpoint.executor)
            .initial_stream_window_size(endpoint.init_stream_window_size)
            .initial_connection_window_size(endpoint.init_connection_window_size)
            .keep_alive_interval(endpoint.http2_keep_alive_interval)
            .clone();

        if let Some(val) = endpoint.http2_keep_alive_timeout {
            settings.keep_alive_timeout(val);
        }

        if let Some(val) = endpoint.http2_keep_alive_while_idle {
            settings.keep_alive_while_idle(val);
        }

        if let Some(val) = endpoint.http2_adaptive_window {
            settings.adaptive_window(val);
        }

        let stack = ServiceBuilder::new()
            .layer_fn(|s| {
                let origin = endpoint.origin.as_ref().unwrap_or(&endpoint.uri).clone();

                AddOrigin::new(s, origin)
            })
            .layer_fn(|s| UserAgent::new(s, endpoint.user_agent.clone()))
            .layer_fn(|s| GrpcTimeout::new(s, endpoint.timeout))
            .option_layer(endpoint.concurrency_limit.map(ConcurrencyLimitLayer::new))
            .option_layer(endpoint.rate_limit.map(|(l, d)| RateLimitLayer::new(l, d)))
            .into_inner();

        let make_service = MakeSendRequestService::new(connector, endpoint, settings);

        let conn = Reconnect::new(make_service, endpoint.uri.clone(), is_lazy);

        Self {
            inner: BoxService::new(stack.layer(conn)),
        }
    }

    pub(crate) async fn connect<C>(connector: C, endpoint: Endpoint) -> Result<Self, crate::Error>
    where
        C: Service<Uri> + Send + 'static,
        C::Error: Into<crate::Error> + Send,
        C::Future: Unpin + Send,
        C::Response: rt::Read + rt::Write + Unpin + Send + 'static,
    {
        Self::new(connector, endpoint, false).ready_oneshot().await
    }

    pub(crate) fn lazy<C>(connector: C, endpoint: Endpoint) -> Self
    where
        C: Service<Uri> + Send + 'static,
        C::Error: Into<crate::Error> + Send,
        C::Future: Unpin + Send,
        C::Response: rt::Read + rt::Write + Unpin + Send + 'static,
    {
        Self::new(connector, endpoint, true)
    }
}

impl Service<Request> for Connection {
    type Response = Response;
    type Error = crate::Error;
    type Future = BoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Service::poll_ready(&mut self.inner, cx).map_err(Into::into)
    }

    fn call(&mut self, req: Request) -> Self::Future {
        self.inner.call(req)
    }
}

impl Load for Connection {
    type Metric = usize;

    fn load(&self) -> Self::Metric {
        0
    }
}

impl fmt::Debug for Connection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Connection").finish()
    }
}

struct SendRequest {
    inner: hyper::client::conn::http2::SendRequest<axum::body::Body>,
}

impl From<hyper::client::conn::http2::SendRequest<axum::body::Body>> for SendRequest {
    fn from(inner: hyper::client::conn::http2::SendRequest<axum::body::Body>) -> Self {
        Self { inner }
    }
}

impl tower::Service<http::Request<axum::body::Body>> for SendRequest {
    type Response = http::Response<axum::body::Body>;
    type Error = crate::Error;
    type Future = BoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx).map_err(Into::into)
    }

    fn call(&mut self, req: http::Request<axum::body::Body>) -> Self::Future {
        let fut = self.inner.send_request(req);

        Box::pin(async move {
            fut.await
                .map_err(Into::into)
                .map(|res| res.map(|body| axum::body::Body::new(body)))
        })
    }
}

struct MakeSendRequestService<C> {
    connector: C,
    endpoint: Endpoint,
    settings: Builder<super::SharedExec>,
}

impl<C> MakeSendRequestService<C> {
    fn new(connector: C, endpoint: Endpoint, settings: Builder<super::SharedExec>) -> Self {
        Self {
            connector,
            endpoint,
            settings,
        }
    }
}

impl<C> tower::Service<Uri> for MakeSendRequestService<C>
where
    C: Service<Uri> + Send + 'static,
    C::Error: Into<crate::Error> + Send,
    C::Future: Unpin + Send,
    C::Response: rt::Read + rt::Write + Unpin + Send + 'static,
{
    type Response = SendRequest;
    type Error = crate::Error;
    type Future = BoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.connector.poll_ready(cx).map_err(Into::into)
    }

    fn call(&mut self, req: Uri) -> Self::Future {
        let fut = self.connector.call(req);
        Box::pin(async move {
            let io = fut.await.map_err(Into::into)?;
            let (send_request, conn) = Builder::new(self.endpoint.executor)
                .initial_stream_window_size(self.endpoint.init_stream_window_size)
                .initial_connection_window_size(self.endpoint.init_connection_window_size)
                .keep_alive_interval(self.endpoint.http2_keep_alive_interval)
                .handshake(io)
                .await?;

            Executor::<BoxFuture<'static, ()>>::execute(
                &self.endpoint.executor,
                Box::pin(async move {
                    if let Err(e) = conn.await {
                        tracing::debug!("connection task error: {:?}", e);
                    }
                }) as _,
            );

            Ok(SendRequest::from(send_request))
        })
    }
}
