//! Compile-valid structural counterexamples, not text-parser approximations.
pub mod api;
pub mod left;
pub mod right;
pub use left::Controller as PublicController;
use left::dispatch as renamed_dispatch;

pub fn run() -> usize {
    let left = PublicController::new();
    let right = right::Controller::new();
    renamed_dispatch(); // @alias_call
    left.handle() + right.handle() // @receiver_calls
}

pub fn function_value() {
    let _callback: fn() = left::dispatch; // @value_not_call
}

pub fn deferred() {
    let _closure = || left::dispatch();
    let _future = async { right::dispatch(); };
}

pub fn through_trait(value: &dyn api::Handler<Output = usize>) -> usize {
    value.handle() // @dynamic_call
}

pub enum Event { Ready, Data { value: usize } }
pub union Packet { pub bits: u32, pub bytes: [u8; 4] }
pub type HandlerAlias = left::Controller;
pub struct Wrapper<T>(pub T);
impl<T> Wrapper<T> { pub fn get(&self) -> &T { &self.0 } }

pub fn outer() {
    fn nested() { left::dispatch(); }
    nested();
}

#[cfg(feature = "alternate")]
pub fn alternate_only() {}
#[cfg(not(feature = "alternate"))]
pub fn default_only() {}

pub const FAKE_CODE: &str = "pub fn phantom() { right::dispatch(); }";
