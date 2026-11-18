use fixture_derive::Marker;
#[derive(Marker)]
pub struct Item;
include!(concat!(env!("OUT_DIR"), "/generated.rs"));
pub const LABEL: &str = include_str!("label.txt");
#[cfg(fixture_active)]
pub fn value() -> usize { shared_left::value() + shared_right::value() + GENERATED }
#[cfg(test)]
mod tests {
    #[test]
    fn execution_marker() {
        std::fs::write(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("TEST_WAS_RUN"), b"ran").unwrap();
    }
}
