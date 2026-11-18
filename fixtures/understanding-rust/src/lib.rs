//! Event output lifecycle. [`events::emit`] owns the wire protocol.
pub mod events;
pub trait Sink {
    /// Write according to the sink protocol.
    fn write(&self, value: usize);
}
pub struct Writer;
impl Writer { pub fn new() -> Self { Self } }
impl Sink for Writer { fn write(&self, value: usize) { events::emit(value); } }
/// Coordinate output through [`events::emit`].
///
/// ```
/// understanding_fixture::run();
/// ```
pub fn run() {
    events::emit(1);
    events::emit(2);
    let callback = events::emit;
    accept(callback);
    let deferred = |_| events::emit(3);
    accept(deferred);
    let _future = async { events::emit(4); };
    let s: &dyn Sink = &Writer::new();
    s.write(5);
}
fn accept<F: Fn(usize)>(f: F) { f(9); }
pub async fn later() { events::emit(6); }
#[cfg(any())]
pub fn inactive() { events::emit(99); }
#[test]
fn protocol_test() { run(); }

const fn const_value() -> usize { 7 }
pub const WIRE_VERSION: usize = const_value();
pub static DEFAULT_VERSION: usize = const_value();

/** A block document with [`events::emit`]. */
pub fn block_documented() {}
