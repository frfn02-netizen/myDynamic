#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatteryState {
    Unknown,
    Charging,
    Discharging,
    Empty,
    FullyCharged,
    PendingCharge,
}

impl BatteryState {
    pub fn from_upower(value: u32) -> Self {
        match value {
            1 => Self::Charging,
            2 => Self::Discharging,
            3 => Self::Empty,
            4 => Self::FullyCharged,
            5 => Self::PendingCharge,
            _ => Self::Unknown,
        }
    }

    pub fn is_charging(self) -> bool {
        matches!(
            self,
            Self::Charging | Self::PendingCharge | Self::FullyCharged
        )
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Charging => "Charging",
            Self::Discharging => "On battery",
            Self::Empty => "Battery empty",
            Self::FullyCharged => "Fully charged",
            Self::PendingCharge => "Pending charge",
            Self::Unknown => "Battery",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BatteryStatus {
    pub percentage: f32,
    pub state: BatteryState,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VolumeStatus {
    pub level: f32,
    pub muted: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NotificationStatus {
    pub app: String,
    pub summary: String,
    pub body: String,
    pub display_ms: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct SystemStatus {
    pub cpu: f32,
    pub ram: f32,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MediaStatus {
    pub title: String,
    pub artist: String,
    pub playing: bool,
    pub length_us: Option<u64>,
    pub position_us: Option<u64>,
    pub art_path: Option<String>,
}

#[derive(Debug, Default)]
pub struct AppState {
    pub battery: Option<BatteryStatus>,
    pub media: Option<MediaStatus>,
    pub volume: Option<VolumeStatus>,
    pub notification: Option<NotificationStatus>,
    pub system: Option<SystemStatus>,
}
