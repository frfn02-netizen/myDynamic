use smithay_client_toolkit::globals::GlobalData;
use smithay_client_toolkit::reexports::client::{Dispatch, QueueHandle};
use wayland_client::globals::GlobalList;
use wayland_client::protocol::wl_region::WlRegion;
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_protocols_plasma::blur::client::org_kde_kwin_blur::OrgKdeKwinBlur;
use wayland_protocols_plasma::blur::client::org_kde_kwin_blur_manager::OrgKdeKwinBlurManager;

/// KDE's compositor-side blur. It is an optional enhancement: if KWin does not
/// advertise the protocol (or the blur effect is disabled), the glass surfaces
/// still render correctly with their own translucent gradients.
pub struct KWinBlur {
    manager: OrgKdeKwinBlurManager,
    blur: OrgKdeKwinBlur,
    surface: WlSurface,
}

impl KWinBlur {
    pub fn bind<State>(
        globals: &GlobalList,
        qh: &QueueHandle<State>,
        surface: &WlSurface,
    ) -> Option<Self>
    where
        State: Dispatch<OrgKdeKwinBlurManager, GlobalData> + Dispatch<OrgKdeKwinBlur, ()> + 'static,
    {
        let manager: OrgKdeKwinBlurManager = globals.bind(qh, 1..=1, GlobalData).ok()?;
        let blur = manager.create(surface, qh, ());
        Some(Self {
            manager,
            blur,
            surface: surface.clone(),
        })
    }

    pub fn set_region(&self, region: Option<&WlRegion>) {
        self.blur.set_region(region);
        self.blur.commit();
    }

    #[allow(dead_code)]
    pub fn unset(&self) {
        self.manager.unset(&self.surface);
        self.blur.release();
    }
}

impl Drop for KWinBlur {
    fn drop(&mut self) {
        self.manager.unset(&self.surface);
        self.blur.release();
    }
}
