use std::cell::RefCell;
use std::rc::Rc;

use slint::ComponentHandle;
use tracing::Level;

mod core;
mod platform;
mod services;
mod ui;

use platform::IslandPlatform;
use platform::wayland::WaylandRuntime;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let startup = std::time::Instant::now();

    tracing_subscriber::fmt()
        .with_max_level(if cfg!(debug_assertions) {
            Level::DEBUG
        } else {
            Level::INFO
        })
        .with_target(false)
        .init();

    let runtime = WaylandRuntime::new()?;
    let adapter = runtime.window_adapter();
    let event_sender = runtime.event_sender();
    let shared = runtime.shared();
    let runtime = Rc::new(RefCell::new(runtime));

    slint::platform::set_platform(Box::new(IslandPlatform::new(runtime.clone(), adapter)))?;

    let ui = ui::Island::new()?;
    ui::bind(&ui, shared.clone());

    runtime.borrow_mut().start(&ui)?;

    #[cfg(debug_assertions)]
    if std::env::var_os("GLASS_ISLAND_DEMO").is_some() {
        runtime.borrow_mut().schedule_demo();
    }

    let (monitor_tx, monitor_rx) = std::sync::mpsc::channel();
    services::battery::spawn(event_sender.clone());
    services::media::spawn(event_sender.clone());
    let volume_control = services::volume::spawn(event_sender.clone());
    services::notifications::spawn(event_sender.clone());
    services::system_monitor::spawn(event_sender, monitor_rx);

    let volume_observer = volume_control;
    shared
        .borrow_mut()
        .add_expansion_observer(Box::new(move |expanded| {
            let _ = volume_observer.send(services::volume::VolumeControl::Expanded(expanded));
        }));
    let monitor_observer = monitor_tx;
    shared
        .borrow_mut()
        .add_expansion_observer(Box::new(move |expanded| {
            let _ = monitor_observer.send(expanded);
        }));

    tracing::info!(
        elapsed_ms = startup.elapsed().as_millis(),
        "Glass Island ready"
    );
    ui.run()?;

    Ok(())
}
