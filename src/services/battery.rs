use std::thread;

use std::collections::HashMap;

use tracing::{debug, warn};
use zbus::Proxy;
use zbus::{Message, OwnedObjectPath, OwnedVariant};

use crate::core::{AppEvent, BatteryState, BatteryStatus, EventSender};

const UPOWER_SERVICE: &str = "org.freedesktop.UPower";
const UPOWER_MANAGER_PATH: &str = "/org/freedesktop/UPower";
const UPOWER_MANAGER_INTERFACE: &str = "org.freedesktop.UPower";
const UPOWER_DEVICE_INTERFACE: &str = "org.freedesktop.UPower.Device";
const DISPLAY_DEVICE_PATH: &str = "/org/freedesktop/UPower/devices/DisplayDevice";

pub fn spawn(tx: EventSender) {
    let result = thread::Builder::new()
        .name("glass-island-upower".into())
        .spawn(move || {
            if let Err(error) = zbus::block_on(run(tx)) {
                warn!(%error, "UPower battery service stopped");
            }
        });

    if let Err(error) = result {
        warn!(%error, "failed to start UPower battery service");
    }
}

async fn run(tx: EventSender) -> zbus::Result<()> {
    let connection = zbus::Connection::system().await?;
    let manager = Proxy::new(
        &connection,
        UPOWER_SERVICE,
        UPOWER_MANAGER_PATH,
        UPOWER_MANAGER_INTERFACE,
    )
    .await?;

    let Some(device_path) = resolve_battery(&connection, &manager).await else {
        let _ = tx.send(AppEvent::Battery(None));
        debug!("UPower reported no present battery device");
        return Ok(());
    };

    debug!(%device_path, "UPower battery device selected");
    let device = Proxy::new(
        &connection,
        UPOWER_SERVICE,
        device_path.as_str(),
        UPOWER_DEVICE_INTERFACE,
    )
    .await?;
    let properties = Proxy::new(
        &connection,
        UPOWER_SERVICE,
        device_path.as_str(),
        "org.freedesktop.DBus.Properties",
    )
    .await?;

    let mut last = None;
    emit(&device, &tx, &mut last).await;

    let mut signals = properties.receive_signal("PropertiesChanged").await?;
    use futures_lite::StreamExt;
    while let Some(message) = signals.next().await {
        if battery_properties_changed(&message) {
            emit(&device, &tx, &mut last).await;
        }
    }

    Ok(())
}

async fn resolve_battery(
    connection: &zbus::Connection,
    manager: &Proxy<'_>,
) -> Option<OwnedObjectPath> {
    let reply = manager.call_method("EnumerateDevices", &()).await.ok()?;
    let devices = reply.body().deserialize::<Vec<OwnedObjectPath>>().ok()?;

    for path in devices {
        let Ok(device) = Proxy::new(
            connection,
            UPOWER_SERVICE,
            path.as_str(),
            UPOWER_DEVICE_INTERFACE,
        )
        .await
        else {
            continue;
        };

        let present = device
            .get_property::<bool>("IsPresent")
            .await
            .unwrap_or(false);
        if !present {
            continue;
        }

        let kind = device.get_property::<u32>("Type").await.unwrap_or(0);
        if kind == 2 {
            return Some(path);
        }
    }

    // Some UPower configurations expose only the composite display device.
    let display = Proxy::new(
        connection,
        UPOWER_SERVICE,
        DISPLAY_DEVICE_PATH,
        UPOWER_DEVICE_INTERFACE,
    )
    .await
    .ok()?;
    if display
        .get_property::<bool>("IsPresent")
        .await
        .unwrap_or(false)
    {
        return OwnedObjectPath::try_from(DISPLAY_DEVICE_PATH).ok();
    }

    None
}

fn battery_properties_changed(message: &Message) -> bool {
    let Ok((_, changed, invalidated)) =
        message
            .body()
            .deserialize::<(String, HashMap<String, OwnedVariant>, Vec<String>)>()
    else {
        return true;
    };

    ["Percentage", "State", "IsPresent"]
        .iter()
        .any(|key| changed.contains_key(*key) || invalidated.iter().any(|name| name == key))
}

async fn emit(device: &Proxy<'_>, tx: &EventSender, last: &mut Option<BatteryStatus>) {
    let percentage = device
        .get_property::<f64>("Percentage")
        .await
        .unwrap_or(0.0);
    let state = device.get_property::<u32>("State").await.unwrap_or(0);

    let status = BatteryStatus {
        percentage: (percentage as f32 / 100.0).clamp(0.0, 1.0),
        state: BatteryState::from_upower(state),
    };

    if last.as_ref() == Some(&status) {
        return;
    }

    *last = Some(status);
    debug!(?status, "battery status updated");
    let _ = tx.send(AppEvent::Battery(Some(status)));
}
