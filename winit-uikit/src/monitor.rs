#![allow(clippy::unnecessary_cast)]

use std::collections::VecDeque;
use std::num::NonZeroU32;
use std::{fmt, hash, ptr};

use dispatch2::{run_on_main, MainThreadBound};
use dpi::PhysicalPosition;
use objc2::rc::Retained;
use objc2::{available, MainThreadMarker, Message};
use objc2_foundation::NSInteger;
use objc2_ui_kit::{UIScreen, UIScreenMode};
use winit_core::monitor::{MonitorHandleProvider, VideoMode};

// Workaround for `MainThreadBound` implementing almost no traits
#[derive(Debug)]
struct MainThreadBoundDelegateImpls<T>(MainThreadBound<Retained<T>>);

impl<T: Message> Clone for MainThreadBoundDelegateImpls<T> {
    fn clone(&self) -> Self {
        Self(run_on_main(|mtm| MainThreadBound::new(Retained::clone(self.0.get(mtm)), mtm)))
    }
}

impl<T: Message> hash::Hash for MainThreadBoundDelegateImpls<T> {
    fn hash<H: hash::Hasher>(&self, state: &mut H) {
        // SAFETY: Marker only used to get the pointer
        let mtm = unsafe { MainThreadMarker::new_unchecked() };
        Retained::as_ptr(self.0.get(mtm)).hash(state);
    }
}

impl<T: Message> PartialEq for MainThreadBoundDelegateImpls<T> {
    fn eq(&self, other: &Self) -> bool {
        // SAFETY: Marker only used to get the pointer
        let mtm = unsafe { MainThreadMarker::new_unchecked() };
        Retained::as_ptr(self.0.get(mtm)) == Retained::as_ptr(other.0.get(mtm))
    }
}

impl<T: Message> Eq for MainThreadBoundDelegateImpls<T> {}

#[derive(Debug, PartialEq, Eq, Hash, Clone)]
pub struct VideoModeHandle {
    pub(crate) mode: VideoMode,
    screen_mode: MainThreadBoundDelegateImpls<UIScreenMode>,
}

impl VideoModeHandle {
    fn new(
        uiscreen: Retained<UIScreen>,
        screen_mode: Retained<UIScreenMode>,
        mtm: MainThreadMarker,
    ) -> VideoModeHandle {
        let refresh_rate_millihertz = refresh_rate_millihertz(&uiscreen);
        let size = screen_mode.size();
        let mode = VideoMode::new(
            (size.width as u32, size.height as u32).into(),
            None,
            refresh_rate_millihertz,
        );

        VideoModeHandle {
            mode,
            screen_mode: MainThreadBoundDelegateImpls(MainThreadBound::new(screen_mode, mtm)),
        }
    }

    pub(super) fn screen_mode(&self, mtm: MainThreadMarker) -> &Retained<UIScreenMode> {
        self.screen_mode.0.get(mtm)
    }
}

pub struct MonitorHandle {
    ui_screen: MainThreadBound<Retained<UIScreen>>,
}

impl MonitorHandleProvider for MonitorHandle {
    fn id(&self) -> u128 {
        self.native_id() as _
    }

    fn native_id(&self) -> u64 {
        // SAFETY: Only getting the pointer.
        let mtm = unsafe { MainThreadMarker::new_unchecked() };
        Retained::as_ptr(self.ui_screen.get(mtm)) as u64
    }

    fn name(&self) -> Option<std::borrow::Cow<'_, str>> {
        run_on_main(|mtm| {
            #[allow(deprecated)]
            let main = UIScreen::mainScreen(mtm);
            if *self.ui_screen(mtm) == main {
                Some("Primary".into())
            } else if Some(self.ui_screen(mtm)) == main.mirroredScreen().as_ref() {
                Some("Mirrored".into())
            } else {
                #[allow(deprecated)]
                UIScreen::screens(mtm)
                    .iter()
                    .position(|rhs| rhs == *self.ui_screen(mtm))
                    .map(|idx| idx.to_string().into())
            }
        })
    }

    fn position(&self) -> Option<PhysicalPosition<i32>> {
        let bounds = self.ui_screen.get_on_main(|ui_screen| ui_screen.nativeBounds());
        Some((bounds.origin.x as f64, bounds.origin.y as f64).into())
    }

    fn scale_factor(&self) -> f64 {
        self.ui_screen.get_on_main(|ui_screen| ui_screen.nativeScale()) as f64
    }

    fn current_video_mode(&self) -> Option<VideoMode> {
        Some(run_on_main(|mtm| {
            VideoModeHandle::new(
                self.ui_screen(mtm).clone(),
                self.ui_screen(mtm).currentMode().unwrap(),
                mtm,
            )
            .mode
        }))
    }

    fn video_modes(&self) -> Box<dyn Iterator<Item = VideoMode>> {
        Box::new(self.video_modes())
    }
}

impl Clone for MonitorHandle {
    fn clone(&self) -> Self {
        run_on_main(|mtm| Self {
            ui_screen: MainThreadBound::new(self.ui_screen.get(mtm).clone(), mtm),
        })
    }
}

impl hash::Hash for MonitorHandle {
    fn hash<H: hash::Hasher>(&self, state: &mut H) {
        // SAFETY: Only getting the pointer.
        let mtm = unsafe { MainThreadMarker::new_unchecked() };
        Retained::as_ptr(self.ui_screen.get(mtm)).hash(state);
    }
}

impl PartialEq for MonitorHandle {
    fn eq(&self, other: &Self) -> bool {
        // SAFETY: Only getting the pointer.
        let mtm = unsafe { MainThreadMarker::new_unchecked() };
        ptr::eq(
            Retained::as_ptr(self.ui_screen.get(mtm)),
            Retained::as_ptr(other.ui_screen.get(mtm)),
        )
    }
}

impl Eq for MonitorHandle {}

impl PartialOrd for MonitorHandle {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for MonitorHandle {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // SAFETY: Only getting the pointer.
        // TODO: Make a better ordering
        let mtm = unsafe { MainThreadMarker::new_unchecked() };
        Retained::as_ptr(self.ui_screen.get(mtm)).cmp(&Retained::as_ptr(other.ui_screen.get(mtm)))
    }
}

impl fmt::Debug for MonitorHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MonitorHandle")
            .field("name", &self.name())
            .field("position", &self.position())
            .field("scale_factor", &self.scale_factor())
            .finish_non_exhaustive()
    }
}

impl MonitorHandle {
    pub(crate) fn new(ui_screen: Retained<UIScreen>) -> Self {
        // Holding `Retained<UIScreen>` implies we're on the main thread.
        let mtm = MainThreadMarker::new().unwrap();
        Self { ui_screen: MainThreadBound::new(ui_screen, mtm) }
    }

    pub fn video_modes_handles(&self) -> impl Iterator<Item = VideoModeHandle> {
        run_on_main(|mtm| {
            let ui_screen = self.ui_screen(mtm);

            ui_screen
                .availableModes()
                .into_iter()
                .map(|mode| VideoModeHandle::new(ui_screen.clone(), mode, mtm))
                .collect::<Vec<_>>()
                .into_iter()
        })
    }

    pub fn video_modes(&self) -> impl Iterator<Item = VideoMode> {
        self.video_modes_handles().map(|handle| handle.mode)
    }

    pub(crate) fn ui_screen(&self, mtm: MainThreadMarker) -> &Retained<UIScreen> {
        self.ui_screen.get(mtm)
    }

    pub fn preferred_video_mode(&self) -> VideoMode {
        run_on_main(|mtm| {
            VideoModeHandle::new(
                self.ui_screen(mtm).clone(),
                self.ui_screen(mtm).preferredMode().unwrap(),
                mtm,
            )
            .mode
        })
    }
}

fn refresh_rate_millihertz(uiscreen: &UIScreen) -> Option<NonZeroU32> {
    let refresh_rate_millihertz: NSInteger = {
        if available!(ios = 10.3, tvos = 10.2) {
            uiscreen.maximumFramesPerSecond()
        } else {
            // https://developer.apple.com/library/archive/technotes/tn2460/_index.html
            // https://en.wikipedia.org/wiki/IPad_Pro#Model_comparison
            //
            // All iOS devices support 60 fps, and on devices where `maximumFramesPerSecond` is not
            // supported, they are all guaranteed to have 60hz refresh rates. This does not
            // correctly handle external displays. ProMotion displays support 120fps, but they were
            // introduced at the same time as the `maximumFramesPerSecond` API.
            //
            // FIXME: earlier OSs could calculate the refresh rate using
            // `-[CADisplayLink duration]`.
            tracing::warn!(
                "`maximumFramesPerSecond` requires iOS 10.3+ or tvOS 10.2+. Defaulting to 60 fps"
            );
            60
        }
    };

    NonZeroU32::new(refresh_rate_millihertz as u32 * 1000)
}

pub fn uiscreens(mtm: MainThreadMarker) -> VecDeque<MonitorHandle> {
    #[allow(deprecated)]
    UIScreen::screens(mtm).into_iter().map(MonitorHandle::new).collect()
}

#[cfg(test)]
mod tests {
    use objc2_foundation::NSSet;

    use super::*;

    // Test that UIScreen pointer comparisons are correct.
    #[test]
    #[allow(deprecated)]
    fn screen_comparisons() {
        // Test code, doesn't matter that it's not thread safe
        let mtm = unsafe { MainThreadMarker::new_unchecked() };

        assert!(ptr::eq(&*UIScreen::mainScreen(mtm), &*UIScreen::mainScreen(mtm)));

        let main = UIScreen::mainScreen(mtm);
        assert!(UIScreen::screens(mtm).iter().any(|screen| ptr::eq(&*screen, &*main)));

        assert!(unsafe {
            NSSet::setWithArray(&UIScreen::screens(mtm)).containsObject(&UIScreen::mainScreen(mtm))
        });
    }
}
