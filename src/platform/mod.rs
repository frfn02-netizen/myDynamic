pub mod kde;
pub mod single_instance;
pub mod wayland;

pub use single_instance::SingleInstance;
pub use wayland::IslandPlatform;
