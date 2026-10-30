use std::collections::HashMap;
use std::thread;
use std::time::Duration;

use futures_lite::{StreamExt, future};
use tracing::{debug, warn};
use zbus::Proxy;
use zbus::{Message, OwnedObjectPath, OwnedVariant};

use crate::core::{AppEvent, BatteryState, BatteryStatus, EventSender};

const UPOWER_SERVICE: &str = "org.freedesktop.UPower";
const UPOWER_MANAGER_PATH: &str = "/org/freedesktop/UPower";
const UPOWER_MANAGER_INTERFACE: &str = "org.freedesktop.UPower";
const UPOWER_DEVICE_INTERFACE: &str = "org.freedesktop.UPower.Device";
const DISPLAY_DEVICE_PATH: &str = "/org/freedesktop/UPower/devices/DisplayDevice";
const RETRY_DELAY: Duration = Duration::from_secs(5);
const NO_DEVICE_RECHECK: Duration = Duration::from_secs(5);
const DEVICE_RECHECK: Duration = Duration::from_secs(30);

enum RunOutcome {
    NoDevice,
    Reconnect,
    ServiceRestarted,
}

enum Wake {
    Battery(Option<zbus::Message>),
    Owner(Option<zbus::Message>),
    Recheck,
}

pub fn spawn(tx: EventSender) {
    let result = thread::Builder::new()
        .name("glass-island-upower".into())
        .spawn(move || run(tx));

    if let Err(error) = result {
        warn!(%error, "failed to start UPower battery service");
    }
}

fn run(tx: EventSender) {
    loop {
        match zbus::block_on(run_once(tx.clone())) {
            Ok(RunOutcome::NoDevice) => {
                debug!("UPower reported no present battery device; rechecking");
                thread::sleep(NO_DEVICE_RECHECK);
            }
            Ok(RunOutcome::Reconnect) => {
                let _ = tx.send(AppEvent::Battery(None));
                thread::sleep(RETRY_DELAY);
            }
            Ok(RunOutcome::ServiceRestarted) => {
                // Keep the last value visible while the replacement service
                // finishes registering its objects.
                thread::sleep(Duration::from_secs(2));
            }
            Err(error) => {
                warn!(%error, "UPower battery service unavailable; retrying");
                let _ = tx.send(AppEvent::Battery(None));
                thread::sleep(RETRY_DELAY);
            }
        }
    }
}

async fn run_once(tx: EventSender) -> zbus::Result<RunOutcome> {
    let connection = zbus::Connection::system().await?;
    let manager = Proxy::new(
        &connection,
        UPOWER_SERVICE,
        UPOWER_MANAGER_PATH,
        UPOWER_MANAGER_INTERFACE,
    )
    .await?;

    let Some(device_path) = resolve_battery(&connection, &manager).await else {
        return Ok(RunOutcome::NoDevice);
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
    emit(&device, &tx, &mut last).await?;

    let mut signals = properties.receive_signal("PropertiesChanged").await?;
    let dbus = Proxy::new(
        &connection,
        "org.freedesktop.DBus",
        "/org/freedesktop/DBus",
        "org.freedesktop.DBus",
    )
    .await?;
    let mut owner_changes = dbus.receive_signal("NameOwnerChanged").await?;

    loop {
        let wake = future::race(
            async {
                future::race(async { Wake::Battery(signals.next().await) }, async {
                    Wake::Owner(owner_changes.next().await)
                })
                .await
            },
            async {
                async_io::Timer::after(DEVICE_RECHECK).await;
                Wake::Recheck
            },
        )
        .await;

        match wake {
            Wake::Battery(Some(message)) => {
                if battery_properties_changed(&message) {
                    emit(&device, &tx, &mut last).await?;
                }
            }
            Wake::Battery(None) => return Ok(RunOutcome::Reconnect),
            Wake::Owner(Some(message)) => {
                let Ok((name, previous, current)) =
                    message.body().deserialize::<(String, String, String)>()
                else {
                    return Ok(RunOutcome::Reconnect);
                };
                if name == UPOWER_SERVICE && previous != current {
                    debug!("UPower service owner changed");
                    return Ok(RunOutcome::ServiceRestarted);
                }
            }
            Wake::Owner(None) => return Ok(RunOutcome::Reconnect),
            Wake::Recheck => {
                // UPower can replace the selected device or disappear without
                // closing the old proxy. Re-enumerate periodically and rebuild
                // the proxies when the device identity changes.
                let Some(current) = resolve_battery(&connection, &manager).await else {
                    let _ = tx.send(AppEvent::Battery(None));
                    return Ok(RunOutcome::NoDevice);
                };
                if current != device_path {
                    return Ok(RunOutcome::Reconnect);
                }
                emit(&device, &tx, &mut last).await?;
            }
        }
    }
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

async fn emit(
    device: &Proxy<'_>,
    tx: &EventSender,
    last: &mut Option<BatteryStatus>,
) -> zbus::Result<()> {
    let percentage = device.get_property::<f64>("Percentage").await?;
    let state = device.get_property::<u32>("State").await?;

    let status = BatteryStatus {
        percentage: (percentage as f32 / 100.0).clamp(0.0, 1.0),
        state: BatteryState::from_upower(state),
    };

    if last.as_ref() == Some(&status) {
        return Ok(());
    }

    *last = Some(status);
    debug!(?status, "battery status updated");
    let _ = tx.send(AppEvent::Battery(Some(status)));
    Ok(())
}
