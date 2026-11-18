use crate::api::Handler;

pub struct Controller { pub value: usize }
impl Controller {
    pub fn new() -> Self { Self { value: 1 } }
    pub fn handle(&self) -> usize { self.value }
}
// The same nominal owner has several impl blocks.
impl Controller { pub fn reset(&mut self) { self.value = 0; } }
impl Handler for Controller {
    type Output = usize;
    fn handle(&self) -> usize { self.value + 10 }
}
pub fn dispatch() {}
pub(crate) fn internal_only() {}
