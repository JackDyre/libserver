use std::{future::Future, io, path::PathBuf, pin::Pin, sync::Arc};

use bytes::Bytes;
use futures::{Stream, TryFutureExt};
use http_body_util::StreamBody;
use hyper::{
    Response, StatusCode,
    body::{Frame, Incoming},
    server::conn::http1,
};
use hyper_util::{rt::TokioIo, service::TowerToHyperService};
use tokio::net::TcpListener;
use tower::{Service as TowerService, util::BoxCloneSyncService};

pub type Request = hyper::Request<Incoming>;

pub type BodyInner = io::Result<Frame<Bytes>>;
pub type BoxedBodyStream = Box<dyn Stream<Item = BodyInner> + Send + Unpin + 'static>;

pub type ServiceResponse = Response<StreamBody<BoxedBodyStream>>;
pub type ServiceError = Box<dyn std::error::Error + Send + Sync>;
pub type ServiceResult = Result<ServiceResponse, ServiceError>;
pub type ServiceBoxFuture = Pin<Box<dyn Future<Output = ServiceResult> + Send + 'static>>;

pub type DynRouter = Arc<dyn Router>;
pub type DynService = BoxCloneSyncService<Request, ServiceResponse, ServiceError>;
pub type DynRoute = Route<DynRouter, DynService>;

pub const NOT_FOUND: StaticService<&str> =
    StaticService::new("404 Not Found", StatusCode::NOT_FOUND);

pub trait Router: Send + Sync + 'static {
    fn matches(&self, req: &Request) -> bool;
}

impl<T> Router for T
where
    T: Fn(&Request) -> bool + Send + Sync + 'static,
{
    fn matches(&self, req: &Request) -> bool {
        self(req)
    }
}

pub struct StaticDirRouter {
    dir: PathBuf,
}

impl StaticDirRouter {
    pub fn new(dir: impl Into<PathBuf>) -> StaticDirRouter {
        StaticDirRouter { dir: dir.into() }
    }
}

impl Router for StaticDirRouter {
    fn matches(&self, req: &Request) -> bool {
        let mut path = self.dir.join(&req.uri().path()[1..]);
        if path.is_dir() {
            path.push("index.html");
        }
        path.is_file()
    }
}

pub struct PathEqRouter {
    path: String,
}

impl PathEqRouter {
    pub fn new(path: impl Into<String>) -> PathEqRouter {
        PathEqRouter { path: path.into() }
    }
}

impl Router for PathEqRouter {
    fn matches(&self, req: &Request) -> bool {
        req.uri().path() == self.path
    }
}

pub struct PathPrefixRouter {
    prefix: String,
}

impl PathPrefixRouter {
    pub fn new(prefix: impl Into<String>) -> PathPrefixRouter {
        PathPrefixRouter {
            prefix: prefix.into(),
        }
    }
}

impl Router for PathPrefixRouter {
    fn matches(&self, req: &Request) -> bool {
        req.uri().path().starts_with(&self.prefix)
    }
}

#[derive(Clone)]
pub struct Route<R, S> {
    router: R,
    service: S,
}

impl<R, S> Route<R, S> {
    pub fn from_parts(router: R, service: S) -> Route<R, S> {
        Route { router, service }
    }

    pub fn map_router<N>(self, f: impl FnOnce(R) -> N) -> Route<N, S> {
        Route::from_parts(f(self.router), self.service)
    }

    pub fn map_service<U>(self, f: impl FnOnce(S) -> U) -> Route<R, U> {
        Route::from_parts(self.router, f(self.service))
    }
}

impl<
    R: Router,
    S: TowerService<Request, Response = ServiceResponse, Error = ServiceError>
        + Send
        + Sync
        + Clone
        + 'static,
> Route<R, S>
where
    S::Future: Send + 'static,
{
    pub fn make_dyn(self) -> DynRoute {
        self.map_router(|r| Arc::new(r) as DynRouter)
            .map_service(|s| BoxCloneSyncService::new(s))
    }
}

pub struct ServiceBuilder {
    routes: Vec<DynRoute>,
}

impl Default for ServiceBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl ServiceBuilder {
    pub fn new() -> ServiceBuilder {
        ServiceBuilder { routes: vec![] }
    }

    pub fn with_route<R, S>(self, route: Route<R, S>) -> ServiceBuilder
    where
        R: Router,
        S: TowerService<Request, Response = ServiceResponse, Error = ServiceError>
            + Clone
            + Send
            + Sync
            + 'static,
        S::Future: Send + 'static,
    {
        self.with_dyn_route(route.make_dyn())
    }

    pub fn with_dyn_route(mut self, route: DynRoute) -> ServiceBuilder {
        self.routes.push(route);
        self
    }

    pub fn with_fallback<S>(self, fallback: S) -> Service
    where
        S: TowerService<Request, Response = ServiceResponse, Error = ServiceError>
            + Clone
            + Send
            + Sync
            + 'static,
        S::Future: Send + 'static,
    {
        Service {
            routes: self.routes,
            fallback: BoxCloneSyncService::new(fallback),
        }
    }

    pub fn with_static_dir<S>(self, dir: impl Into<PathBuf>, service: S) -> Self
    where
        S: TowerService<Request, Response = ServiceResponse, Error = ServiceError>
            + Send
            + Sync
            + Clone
            + 'static,
        S::Future: Send + 'static,
    {
        self.with_route(Route::from_parts(
            StaticDirRouter { dir: dir.into() },
            service,
        ))
    }
}

#[derive(Clone)]
pub struct Service {
    routes: Vec<DynRoute>,
    fallback: DynService,
}

impl Service {
    pub async fn serve(self, listener: TcpListener) -> Result<(), std::io::Error> {
        let adapter = TowerToHyperService::new(self);
        let service = Arc::new(adapter);

        loop {
            let service = service.clone();
            let io = TokioIo::new(listener.accept().await?.0);

            tokio::spawn(async move {
                http1::Builder::new()
                    .serve_connection(io, service)
                    .with_upgrades()
                    .await
                    .inspect_err(|e| {
                        dbg!(e);
                    })
                    .ok();
            });
        }
    }
}

impl TowerService<Request> for Service {
    type Response = ServiceResponse;
    type Error = ServiceError;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(
        &mut self,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: Request) -> Self::Future {
        Box::pin(
            self.routes
                .iter_mut()
                .find(|r| r.router.matches(&req))
                .map(|r| &mut r.service)
                .unwrap_or(&mut self.fallback)
                .call(req)
                .inspect_err(|e| println!("{e}")),
        )
    }
}

pub fn single_frame_body(body: impl Into<Bytes> + Send + 'static) -> StreamBody<BoxedBodyStream> {
    make_body_from_stream(single_frame_stream(body))
}

pub fn make_frame(frame: impl Into<Bytes>) -> BodyInner {
    Ok(Frame::data(frame.into()))
}

pub fn single_frame_stream(body: impl Into<Bytes>) -> impl Stream<Item = BodyInner> {
    futures::stream::once(async { make_frame(body) })
}

pub fn make_body_from_stream<S>(stream: S) -> StreamBody<BoxedBodyStream>
where
    S: Stream<Item = BodyInner> + Send + 'static,
{
    StreamBody::new(Box::new(Box::pin(stream)))
}

pub fn static_service(
    body: impl Into<Bytes> + Send + 'static,
) -> impl TowerService<
    Request,
    Response = ServiceResponse,
    Error = ServiceError,
    Future = ServiceBoxFuture,
> + Clone
+ Send
+ 'static {
    let body = body.into();
    tower::service_fn(move |_| {
        let body = body.clone();
        let fut = async move { Ok(Response::new(single_frame_body(body))) };
        Box::pin(fut) as ServiceBoxFuture
    })
}

#[derive(Clone)]
pub struct StaticService<T> {
    body: T,
    status: StatusCode,
}

impl<T> StaticService<T> {
    pub const fn new(body: T, status: StatusCode) -> Self {
        Self { body, status }
    }
}

impl<T: Into<Bytes> + Clone + Send + 'static> TowerService<Request> for StaticService<T> {
    type Response = ServiceResponse;
    type Error = ServiceError;
    type Future = ServiceBoxFuture;

    fn poll_ready(
        &mut self,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn call(&mut self, _req: Request) -> Self::Future {
        let mut resp = Response::new(single_frame_body(self.body.clone()));
        *resp.status_mut() = self.status;
        Box::pin(async { Ok(resp) })
    }
}
