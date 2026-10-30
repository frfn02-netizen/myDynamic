use crate::core::state::{
    BatteryStatus, MediaStatus, NotificationStatus, SystemStatus, VolumeStatus,
};

#[derive(Debug, Clone)]
pub enum AppEvent {
    Battery(Option<BatteryStatus>),
    Media(Option<MediaStatus>),
    Volume(Option<VolumeStatus>),
    Notification(Option<NotificationStatus>),
    System(Option<SystemStatus>),
}

pub type EventSender = calloop::channel::Sender<AppEvent>;
pub type EventChannel = calloop::channel::Channel<AppEvent>;

pub fn channel() -> (EventSender, EventChannel) {
    calloop::channel::channel()
}
