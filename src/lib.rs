pub struct Request;

pub trait Router {
    type Context;

    fn run(&mut self, request: &mut Request) -> Option<Self::Context>;
}
