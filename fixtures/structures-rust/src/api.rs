pub trait Handler {
    type Output;
    fn handle(&self) -> Self::Output;
}
