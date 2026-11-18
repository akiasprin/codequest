#[cfg(feature = "left")]
pub fn value() -> usize { 1 }
#[cfg(not(feature = "left"))]
pub fn value() -> usize { 2 }
