use std::cell::RefCell;
use std::rc::Rc;

use tracing::debug;

use crate::platform::wayland::Shared;

slint::include_modules!();

pub fn bind(ui: &Island, shared: Rc<RefCell<Shared>>) {
    let toggle_shared = shared.clone();
    ui.on_toggle_requested(move || {
        debug!("toggle requested from UI");
        toggle_shared.borrow_mut().toggle();
    });

    let close_shared = shared;
    ui.on_close_requested(move || {
        debug!("close requested from UI");
        close_shared.borrow_mut().quit();
    });
}

#[cfg(test)]
mod tests {
    use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
    use slint::platform::{Platform, PlatformError, WindowAdapter, WindowEvent};
    use slint::{ComponentHandle, LogicalPosition, LogicalSize};
    use std::cell::Cell;
    use std::rc::Rc;

    use super::Island;

    struct TestPlatform;

    impl Platform for TestPlatform {
        fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
            let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
            Ok(window as Rc<dyn WindowAdapter>)
        }
    }

    fn ensure_platform() {
        // Slint's platform context is per-thread; tests may run on separate
        // threads, so each thread installs its own minimal platform.
        let _ = slint::platform::set_platform(Box::new(TestPlatform));
    }

    fn make_ui() -> (Island, Rc<Cell<u32>>, Rc<Cell<u32>>) {
        ensure_platform();
        let ui = Island::new().expect("Island component");
        let toggles = Rc::new(Cell::new(0));
        let closes = Rc::new(Cell::new(0));

        let toggle_counter = toggles.clone();
        ui.on_toggle_requested(move || toggle_counter.set(toggle_counter.get() + 1));
        let close_counter = closes.clone();
        ui.on_close_requested(move || close_counter.set(close_counter.get() + 1));

        ui.window().set_size(LogicalSize::new(420.0, 180.0));
        (ui, toggles, closes)
    }

    fn click(ui: &Island, x: f32, y: f32) {
        let position = LogicalPosition::new(x, y);
        let window = ui.window();
        window.dispatch_event(WindowEvent::PointerMoved { position });
        window.dispatch_event(WindowEvent::PointerPressed {
            position,
            button: slint::platform::PointerEventButton::Left,
        });
        window.dispatch_event(WindowEvent::PointerReleased {
            position,
            button: slint::platform::PointerEventButton::Left,
        });
    }

    #[test]
    fn pill_click_toggles_and_motion_does_not() {
        let (ui, toggles, _closes) = make_ui();

        ui.window().dispatch_event(WindowEvent::PointerMoved {
            position: LogicalPosition::new(210.0, 29.0),
        });
        assert_eq!(toggles.get(), 0, "motion must not toggle state");

        click(&ui, 210.0, 29.0);
        assert_eq!(toggles.get(), 1, "a click in the compact pill must toggle");
    }

    #[test]
    fn expanded_close_button_does_not_toggle() {
        let (ui, toggles, closes) = make_ui();
        ui.set_expansion(1.0);
        ui.window().request_redraw();

        // Close button center in expanded state: pill x=14, expanded x=358, y=58+12+11.
        click(&ui, 383.0, 81.0);
        assert_eq!(closes.get(), 1, "close button must invoke close-requested");
        assert_eq!(toggles.get(), 0, "close button click must not toggle");
    }

    #[test]
    fn expanded_body_click_toggles_but_host_outside_pill_does_not() {
        let (ui, toggles, closes) = make_ui();
        ui.set_expansion(1.0);
        ui.window().request_redraw();

        click(&ui, 200.0, 105.0);
        assert_eq!(toggles.get(), 1, "expanded pill body must toggle");
        assert_eq!(closes.get(), 0);

        // Expanded pill occupies x=14..406, y=0..164. The host corner is
        // outside it and must never receive a UI click.
        click(&ui, 5.0, 170.0);
        assert_eq!(toggles.get(), 1, "outside-pill host area must be inert");
    }

    #[test]
    fn drag_out_of_pill_cancels_click() {
        let (ui, toggles, closes) = make_ui();
        click(&ui, 210.0, 29.0);
        assert_eq!(toggles.get(), 1);

        let window = ui.window();
        let press = LogicalPosition::new(210.0, 29.0);
        window.dispatch_event(WindowEvent::PointerMoved { position: press });
        window.dispatch_event(WindowEvent::PointerPressed {
            position: press,
            button: slint::platform::PointerEventButton::Left,
        });
        window.dispatch_event(WindowEvent::PointerMoved {
            position: LogicalPosition::new(5.0, 170.0),
        });
        window.dispatch_event(WindowEvent::PointerReleased {
            position: LogicalPosition::new(5.0, 170.0),
            button: slint::platform::PointerEventButton::Left,
        });
        assert_eq!(toggles.get(), 1, "release outside the pill must not click");
        assert_eq!(closes.get(), 0);
    }

    #[test]
    fn hit_testing_stays_logical_at_fractional_scale() {
        ensure_platform();
        let ui = Island::new().expect("Island component");
        let toggles = Rc::new(Cell::new(0));
        let counter = toggles.clone();
        ui.on_toggle_requested(move || counter.set(counter.get() + 1));
        ui.window()
            .dispatch_event(WindowEvent::ScaleFactorChanged { scale_factor: 1.5 });
        ui.window().set_size(LogicalSize::new(420.0, 180.0));

        click(&ui, 210.0, 29.0);
        assert_eq!(
            toggles.get(),
            1,
            "surface-local logical coordinates must drive hit testing"
        );
    }
}
