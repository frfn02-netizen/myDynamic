mod event_bus;
mod state;

pub use event_bus::{AppEvent, EventSender, channel};
pub use state::{
    AppState, BatteryState, BatteryStatus, MediaStatus, NotificationStatus, SystemStatus,
    VolumeStatus,
};
