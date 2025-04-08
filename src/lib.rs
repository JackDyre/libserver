use std::{io, sync::Arc};

use bytes::Bytes;
use futures::{Stream, future::BoxFuture};
use http::HeaderMap;
use http_body_util::{BodyExt, StreamBody};
use hyper::body::Frame;

pub struct Request {
    pub header: Header,
    pub body: Body,
}

impl Request {
    pub fn split(self) -> (Header, Body) {
        (self.header, self.body)
    }
}

pub struct Body {
    inner: hyper::Request<hyper::body::Incoming>,
}

impl Body {
    pub async fn collect_bytes(self, max_size: Option<usize>) -> Result<Bytes, Error> {
        let body = self.inner.collect().await?.to_bytes();
        if let Some(max) = max_size {
            if body.len() > max {
                return Err(Error::RequestTooLarge(body.len(), max));
            }
        }
        Ok(body)
    }

    pub async fn collect_str(self, max_size: Option<usize>) -> Result<String, Error> {
        let body = String::from_utf8(self.collect_bytes(max_size).await?.to_vec())?;
        Ok(body)
    }
}

impl From<hyper::Request<hyper::body::Incoming>> for Body {
    fn from(value: hyper::Request<hyper::body::Incoming>) -> Self {
        Body { inner: value }
    }
}

impl From<hyper::Request<hyper::body::Incoming>> for Request {
    fn from(value: hyper::Request<hyper::body::Incoming>) -> Self {
        Request {
            header: Header {
                url_path: value.uri().path().to_string(),
                headers: value.headers().to_owned(),
            },
            body: value.into(),
        }
    }
}

pub struct Header {
    pub url_path: String,
    pub headers: HeaderMap,
}

pub struct Response {
    status: usize,
    headers: HeaderMap,
    inner: StreamBody<Box<dyn Stream<Item = io::Result<Frame<Bytes>>> + Send + Unpin + 'static>>,
}

impl Response {
    fn to_hyper(
        self,
    ) -> hyper::Response<
        StreamBody<Box<dyn Stream<Item = io::Result<Frame<Bytes>>> + Send + Unpin + 'static>>,
    > {
        todo!()
    }
}

pub enum Error {
    Hyper(hyper::Error),
    Encoding(std::string::FromUtf8Error),
    RequestTooLarge(usize, usize),
}

impl From<hyper::Error> for Error {
    fn from(value: hyper::Error) -> Self {
        Error::Hyper(value)
    }
}

impl From<std::string::FromUtf8Error> for Error {
    fn from(value: std::string::FromUtf8Error) -> Self {
        Error::Encoding(value)
    }
}

pub trait Router {
    type Context;

    fn run(&self, request: &mut Request) -> BoxFuture<'static, Option<Self::Context>>;
}

pub trait Service {
    type Context;

    fn call(
        &self,
        request: Request,
        cx: &Self::Context,
    ) -> BoxFuture<'static, Result<Response, Error>>;
}

struct ContextService<'a, S, C> {
    service: &'a S,
    context: &'a C,
}

impl<C, S: Service<Context = C>> tower::Service<hyper::Request<hyper::body::Incoming>>
    for ContextService<'_, S, C>
{
    type Response = Response;
    type Error = Error;
    type Future = BoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn call(&mut self, req: hyper::Request<hyper::body::Incoming>) -> Self::Future {
        Service::call(self.service, req.into(), self.context)
    }

    fn poll_ready(
        &mut self,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        std::task::Poll::Ready(Ok(()))
    }
}

impl<C, S: Service<Context = C>> hyper::service::Service<hyper::Request<hyper::body::Incoming>>
    for ContextService<'_, S, C>
{
    type Response = Response;
    type Error = Error;
    type Future = BoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn call(&self, req: hyper::Request<hyper::body::Incoming>) -> Self::Future {
        Service::call(self.service, req.into(), self.context)
    }
}

pub struct Route<R, S> {
    router: R,
    service: S,
}

impl<
    C: Send + Sync + 'static,
    R: Router<Context = C> + Send + Sync + 'static,
    S: Service<Context = C> + Send + Sync + 'static,
> Route<R, S>
{
    pub fn homogeneous(self) -> HomogeneousRoute {
        let intermediate = HomogeneousRouteIntermediate {
            router: Arc::new(self.router) as Arc<dyn Router<Context = C> + Send + Sync + 'static>,
            service: Arc::new(self.service)
                as Arc<dyn Service<Context = C> + Send + Sync + 'static>,
        };

        intermediate.make_fully_homogeneous()
    }
}

pub struct HomogeneousRouteIntermediate<C> {
    router: Arc<dyn Router<Context = C> + Send + Sync + 'static>,
    service: Arc<dyn Service<Context = C> + Send + Sync + 'static>,
}

impl<C: Send + Sync + 'static> HomogeneousRouteIntermediate<C> {
    pub fn as_route_fn(
        self,
    ) -> impl Fn(Request) -> BoxFuture<'static, Result<Result<Response, Error>, Request>> {
        let this = Arc::new(self);
        move |mut req: Request| {
            let this = this.clone();
            Box::pin(async move {
                match this.router.run(&mut req).await {
                    Some(cx) => Ok(this.service.call(req, &cx).await),
                    None => Err(req),
                }
            })
        }
    }

    pub fn make_fully_homogeneous(self) -> HomogeneousRoute {
        let x = Arc::new(self.as_route_fn());

        HomogeneousRoute { inner: x }
    }
}

pub struct HomogeneousRoute {
    inner: Arc<dyn Fn(Request) -> BoxFuture<'static, Result<Result<Response, Error>, Request>>>,
}

