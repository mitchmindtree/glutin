#![cfg(target_os = "macos")]

use {CreationError, Event, MouseCursor, CursorState};
use CreationError::OsError;
use libc;

use ContextError;
use GlAttributes;
use GlContext;
use PixelFormat;
use PixelFormatRequirements;
use Robustness;
use WindowAttributes;
use native_monitor::NativeMonitorId;
use os::macos::ActivationPolicy;

use objc::runtime::{Class, Object, Sel, BOOL, YES, NO};
use objc::declare::ClassDecl;

use cgl::{CGLEnable, kCGLCECrashOnRemovedFunctions, CGLSetParameter, kCGLCPSurfaceOpacity};

use cocoa::base::{id, nil};
use cocoa::foundation::{NSAutoreleasePool, NSDate, NSDefaultRunLoopMode, NSPoint, NSRect, NSSize,
                        NSString, NSUInteger};
use cocoa::appkit::{self, NSApplication, NSEvent, NSOpenGLContext, NSOpenGLPixelFormat, NSView, NSWindow};

use core_foundation::base::TCFType;
use core_foundation::string::CFString;
use core_foundation::bundle::{CFBundleGetBundleWithIdentifier, CFBundleGetFunctionPointerForName};

use core_graphics::display::{CGAssociateMouseAndMouseCursorPosition, CGMainDisplayID, CGDisplayPixelsHigh, CGWarpMouseCursorPosition};

use std::ffi::CStr;
use std::collections::VecDeque;
use std::str::FromStr;
use std::str::from_utf8;
use std::sync::Mutex;
use std::ops::Deref;

use events::ElementState;
use events::{self, MouseButton, TouchPhase};

pub use self::monitor::{MonitorId, get_available_monitors, get_primary_monitor};
pub use self::headless::HeadlessContext;
pub use self::headless::PlatformSpecificHeadlessBuilderAttributes;

mod monitor;
mod event;
mod headless;
mod helpers;

static mut shift_pressed: bool = false;
static mut ctrl_pressed: bool = false;
static mut win_pressed: bool = false;
static mut alt_pressed: bool = false;

struct DelegateState {
    context: IdRef,
    view: IdRef,
    window: IdRef,
    resize_handler: Option<fn(u32, u32)>,

    /// Events that have been retreived with XLib but not dispatched with iterators yet
    pending_events: Mutex<VecDeque<Event>>,
}

struct WindowDelegate {
    state: Box<DelegateState>,
    _this: IdRef,
}

impl WindowDelegate {
    /// Get the delegate class, initiailizing it neccessary
    fn class() -> *const Class {
        use std::os::raw::c_void;
        use std::sync::{Once, ONCE_INIT};

        extern fn window_should_close(this: &Object, _: Sel, _: id) -> BOOL {
            unsafe {
                let state: *mut c_void = *this.get_ivar("glutinState");
                let state = state as *mut DelegateState;
                (*state).pending_events.lock().unwrap().push_back(Event::Closed);
            }
            YES
        }

        extern fn window_did_resize(this: &Object, _: Sel, _: id) {
            unsafe {
                let state: *mut c_void = *this.get_ivar("glutinState");
                let state = &mut *(state as *mut DelegateState);

                let _: () = msg_send![*state.context, update];

                if let Some(handler) = state.resize_handler {
                    let rect = NSView::frame(*state.view);
                    let scale_factor = NSWindow::backingScaleFactor(*state.window) as f32;
                    (handler)((scale_factor * rect.size.width as f32) as u32,
                              (scale_factor * rect.size.height as f32) as u32);
                }
            }
        }

        extern fn window_did_become_key(this: &Object, _: Sel, _: id) {
            unsafe {
                // TODO: center the cursor if the window had mouse grab when it
                // lost focus

                let state: *mut c_void = *this.get_ivar("glutinState");
                let state = state as *mut DelegateState;
                (*state).pending_events.lock().unwrap().push_back(Event::Focused(true));
            }
        }

        extern fn window_did_resign_key(this: &Object, _: Sel, _: id) {
            unsafe {
                let state: *mut c_void = *this.get_ivar("glutinState");
                let state = state as *mut DelegateState;
                (*state).pending_events.lock().unwrap().push_back(Event::Focused(false));
            }
        }

        static mut delegate_class: *const Class = 0 as *const Class;
        static INIT: Once = ONCE_INIT;

        INIT.call_once(|| unsafe {
            // Create new NSWindowDelegate
            let superclass = Class::get("NSObject").unwrap();
            let mut decl = ClassDecl::new("GlutinWindowDelegate", superclass).unwrap();

            // Add callback methods
            decl.add_method(sel!(windowShouldClose:),
                window_should_close as extern fn(&Object, Sel, id) -> BOOL);
            decl.add_method(sel!(windowDidResize:),
                window_did_resize as extern fn(&Object, Sel, id));

            decl.add_method(sel!(windowDidBecomeKey:),
                window_did_become_key as extern fn(&Object, Sel, id));
            decl.add_method(sel!(windowDidResignKey:),
                window_did_resign_key as extern fn(&Object, Sel, id));

            // Store internal state as user data
            decl.add_ivar::<*mut c_void>("glutinState");

            delegate_class = decl.register();
        });

        unsafe {
            delegate_class
        }
    }

    fn new(state: DelegateState) -> WindowDelegate {
        // Box the state so we can give a pointer to it
        let mut state = Box::new(state);
        let state_ptr: *mut DelegateState = &mut *state;
        unsafe {
            let delegate = IdRef::new(msg_send![WindowDelegate::class(), new], "delegate");

            (&mut **delegate).set_ivar("glutinState", state_ptr as *mut ::std::os::raw::c_void);
            let _: () = msg_send![*state.window, setDelegate:*delegate];

            WindowDelegate { state: state, _this: delegate }
        }
    }
}

impl Drop for WindowDelegate {
    fn drop(&mut self) {
        unsafe {
            println!("drop: WindowDelegate ... ");
            // Nil the window's delegate so it doesn't still reference us
            let _: () = msg_send![*self.state.window, setDelegate:nil];
            println!("drop finished");
        }
    }
}

#[derive(Clone, Default)]
pub struct PlatformSpecificWindowBuilderAttributes {
    pub activation_policy: ActivationPolicy,
}

pub struct Window {
    view: IdRef,
    window: IdRef,
    context: IdRef,
    pixel_format: PixelFormat,
    delegate: WindowDelegate,
}

unsafe impl Send for Window {}
unsafe impl Sync for Window {}

#[derive(Clone)]
pub struct WindowProxy;

impl WindowProxy {
    pub fn wakeup_event_loop(&self) {
        unsafe {
            let pool = NSAutoreleasePool::new(nil);
            let event =
                NSEvent::otherEventWithType_location_modifierFlags_timestamp_windowNumber_context_subtype_data1_data2_(
                    nil, appkit::NSApplicationDefined, NSPoint::new(0.0, 0.0), appkit::NSEventModifierFlags::empty(),
                    0.0, 0, nil, appkit::NSEventSubtype::NSApplicationActivatedEventType, 0, 0);
            appkit::NSApp().postEvent_atStart_(event, NO);
            pool.drain();
        }
    }
}

pub struct PollEventsIterator<'a> {
    window: &'a Window,
}

impl<'a> Iterator for PollEventsIterator<'a> {
    type Item = Event;

    fn next(&mut self) -> Option<Event> {
        if let Some(ev) = self.window.delegate.state.pending_events.lock().unwrap().pop_front() {
            return Some(ev);
        }

        let event: Option<Event>;
        unsafe {
            let pool = NSAutoreleasePool::new(nil);

            let nsevent = appkit::NSApp().nextEventMatchingMask_untilDate_inMode_dequeue_(
                appkit::NSAnyEventMask.bits() | appkit::NSEventMaskPressure.bits(),
                NSDate::distantPast(nil),
                NSDefaultRunLoopMode,
                YES);
            event = NSEventToEvent(self.window, nsevent);

            let _: () = msg_send![pool, release];
        }
        event
    }
}

pub struct WaitEventsIterator<'a> {
    window: &'a Window,
}

impl<'a> Iterator for WaitEventsIterator<'a> {
    type Item = Event;

    fn next(&mut self) -> Option<Event> {
        if let Some(ev) = self.window.delegate.state.pending_events.lock().unwrap().pop_front() {
            return Some(ev);
        }

        let event: Option<Event>;
        unsafe {
            let pool = NSAutoreleasePool::new(nil);

            let nsevent = appkit::NSApp().nextEventMatchingMask_untilDate_inMode_dequeue_(
                appkit::NSAnyEventMask.bits() | appkit::NSEventMaskPressure.bits(),
                NSDate::distantFuture(nil),
                NSDefaultRunLoopMode,
                YES);
            event = NSEventToEvent(self.window, nsevent);

            let _: () = msg_send![pool, release];
        }

        if event.is_none() {
            return Some(Event::Awakened);
        } else {
            return event;
        }
    }
}

impl Window {
    pub fn new(win_attribs: &WindowAttributes, pf_reqs: &PixelFormatRequirements,
               opengl: &GlAttributes<&Window>,
               pl_attribs: &PlatformSpecificWindowBuilderAttributes)
               -> Result<Window, CreationError>
    {
        if opengl.sharing.is_some() {
            unimplemented!()
        }

        // not implemented
        assert!(win_attribs.min_dimensions.is_none());
        assert!(win_attribs.max_dimensions.is_none());

        match opengl.robustness {
            Robustness::RobustNoResetNotification | Robustness::RobustLoseContextOnReset => {
                return Err(CreationError::RobustnessNotSupported);
            },
            _ => ()
        }

        let app = match Window::create_app(pl_attribs.activation_policy) {
            Some(app) => app,
            None      => { return Err(OsError(format!("Couldn't create NSApplication"))); },
        };

        let window = match Window::create_window(win_attribs)
        {
            Some(window) => window,
            None         => { return Err(OsError(format!("Couldn't create NSWindow"))); },
        };
        let view = match Window::create_view(*window) {
            Some(view) => view,
            None       => { return Err(OsError(format!("Couldn't create NSView"))); },
        };

        // TODO: perhaps we should return error from create_context so we can
        // determine the cause of failure and possibly recover?
        let (context, pf) = match Window::create_context(*view, pf_reqs, opengl) {
            Ok((context, pf)) => (context, pf),
            Err(e) => { return Err(OsError(format!("Couldn't create OpenGL context: {}", e))); },
        };

        unsafe {
            if win_attribs.transparent {
                let clear_col = {
                    let cls = Class::get("NSColor").unwrap();

                    msg_send![cls, clearColor]
                };
                window.setOpaque_(NO);
                window.setBackgroundColor_(clear_col);

                let obj = context.CGLContextObj();

                let mut opacity = 0;
                CGLSetParameter(obj as *mut _, kCGLCPSurfaceOpacity, &mut opacity);
            }

            app.activateIgnoringOtherApps_(YES);
            if win_attribs.visible {
                window.makeKeyAndOrderFront_(nil);
            } else {
                window.makeKeyWindow();
            }
        }

        let ds = DelegateState {
            context: context.clone(),
            view: view.clone(),
            window: window.clone(),
            resize_handler: None,
            pending_events: Mutex::new(VecDeque::new()),
        };

        let window = Window {
            view: view,
            window: window,
            context: context,
            pixel_format: pf,
            delegate: WindowDelegate::new(ds),
        };

        Ok(window)
    }

    fn create_app(activation_policy: ActivationPolicy) -> Option<id> {
        unsafe {
            let app = appkit::NSApp();
            if app == nil {
                None
            } else {
                app.setActivationPolicy_(activation_policy.into());
                app.finishLaunching();
                Some(app)
            }
        }
    }

    fn create_window(attrs: &WindowAttributes) -> Option<IdRef> {
        unsafe {
            let screen = match attrs.monitor {
                Some(ref monitor_id) => {
                    let native_id = match monitor_id.get_native_identifier() {
                        NativeMonitorId::Numeric(num) => num,
                        _ => panic!("OS X monitors should always have a numeric native ID")
                    };
                    let matching_screen = {
                        let screens = appkit::NSScreen::screens(nil);
                        let count: NSUInteger = msg_send![screens, count];
                        let key = IdRef::new(NSString::alloc(nil).init_str("NSScreenNumber"), "key");
                        let mut matching_screen: Option<id> = None;
                        for i in 0..count {
                            let screen = msg_send![screens, objectAtIndex:i as NSUInteger];
                            let device_description = appkit::NSScreen::deviceDescription(screen);
                            let value: id = msg_send![device_description, objectForKey:*key];
                            if value != nil {
                                let screen_number: NSUInteger = msg_send![value, unsignedIntegerValue];
                                if screen_number as u32 == native_id {
                                    matching_screen = Some(screen);
                                    break;
                                }
                            }
                        }
                        matching_screen
                    };
                    Some(matching_screen.unwrap_or(appkit::NSScreen::mainScreen(nil)))
                },
                None => None
            };
            let frame = match screen {
                Some(screen) => appkit::NSScreen::frame(screen),
                None => {
                    let (width, height) = attrs.dimensions.unwrap_or((800, 600));
                    NSRect::new(NSPoint::new(0., 0.), NSSize::new(width as f64, height as f64))
                }
            };

            let masks = if screen.is_some() || attrs.transparent {
                // Fullscreen or transparent window
                appkit::NSBorderlessWindowMask as NSUInteger |
                appkit::NSResizableWindowMask as NSUInteger |
                appkit::NSTitledWindowMask as NSUInteger
            } else if attrs.decorations {
                // Classic opaque window with titlebar
                appkit::NSClosableWindowMask as NSUInteger |
                appkit::NSMiniaturizableWindowMask as NSUInteger |
                appkit::NSResizableWindowMask as NSUInteger |
                appkit::NSTitledWindowMask as NSUInteger
            } else {
                // Opaque window without a titlebar
                appkit::NSClosableWindowMask as NSUInteger |
                appkit::NSMiniaturizableWindowMask as NSUInteger |
                appkit::NSResizableWindowMask as NSUInteger |
                appkit::NSTitledWindowMask as NSUInteger |
                appkit::NSFullSizeContentViewWindowMask as NSUInteger
            };

            let window = IdRef::new(NSWindow::alloc(nil).initWithContentRect_styleMask_backing_defer_(
                frame,
                masks,
                appkit::NSBackingStoreBuffered,
                NO,
            ), "window");
            window.non_nil().map(|window| {
                let title = IdRef::new(NSString::alloc(nil).init_str(&attrs.title), "title");
                window.setTitle_(*title);
                window.setAcceptsMouseMovedEvents_(YES);

                if !attrs.decorations {
                    window.setTitleVisibility_(appkit::NSWindowTitleVisibility::NSWindowTitleHidden);
                    window.setTitlebarAppearsTransparent_(YES);
                }

                if screen.is_some() {
                    window.setLevel_(appkit::NSMainMenuWindowLevel as i64 + 1);
                }
                else {
                    window.center();
                }
                window
            })
        }
    }

    fn create_view(window: id) -> Option<IdRef> {
        unsafe {
            let view = IdRef::new(NSView::alloc(nil).init(), "view");
            view.non_nil().map(|view| {
                view.setWantsBestResolutionOpenGLSurface_(YES);
                window.setContentView_(*view);
                view
            })
        }
    }

    fn create_context(view: id, pf_reqs: &PixelFormatRequirements, opengl: &GlAttributes<&Window>)
                      -> Result<(IdRef, PixelFormat), CreationError>
    {
        let attributes = try!(helpers::build_nsattributes(pf_reqs, opengl));
        unsafe {
            let pixelformat = IdRef::new(NSOpenGLPixelFormat::alloc(nil).initWithAttributes_(&attributes), "pixel_format");

            if let Some(pixelformat) = pixelformat.non_nil() {

                // TODO: Add context sharing
                let context = IdRef::new(NSOpenGLContext::alloc(nil).initWithFormat_shareContext_(*pixelformat, nil), "context");

                if let Some(cxt) = context.non_nil() {
                    let pf = {
                        let get_attr = |attrib: appkit::NSOpenGLPixelFormatAttribute| -> i32 {
                            let mut value = 0;

                            NSOpenGLPixelFormat::getValues_forAttribute_forVirtualScreen_(
                                *pixelformat,
                                &mut value,
                                attrib,
                                NSOpenGLContext::currentVirtualScreen(*cxt));

                            value
                        };

                        PixelFormat {
                            hardware_accelerated: get_attr(appkit::NSOpenGLPFAAccelerated) != 0,
                            color_bits: (get_attr(appkit::NSOpenGLPFAColorSize) - get_attr(appkit::NSOpenGLPFAAlphaSize)) as u8,
                            alpha_bits: get_attr(appkit::NSOpenGLPFAAlphaSize) as u8,
                            depth_bits: get_attr(appkit::NSOpenGLPFADepthSize) as u8,
                            stencil_bits: get_attr(appkit::NSOpenGLPFAStencilSize) as u8,
                            stereoscopy: get_attr(appkit::NSOpenGLPFAStereo) != 0,
                            double_buffer: get_attr(appkit::NSOpenGLPFADoubleBuffer) != 0,
                            multisampling: if get_attr(appkit::NSOpenGLPFAMultisample) > 0 {
                                Some(get_attr(appkit::NSOpenGLPFASamples) as u16)
                            } else {
                                None
                            },
                            srgb: true,
                        }
                    };

                    cxt.setView_(view);
                    let value = if opengl.vsync { 1 } else { 0 };
                    cxt.setValues_forParameter_(&value, appkit::NSOpenGLContextParameter::NSOpenGLCPSwapInterval);

                    CGLEnable(cxt.CGLContextObj() as *mut _, kCGLCECrashOnRemovedFunctions);

                    Ok((cxt, pf))
                } else {
                    Err(CreationError::NotSupported)
                }
            } else {
                Err(CreationError::NoAvailablePixelFormat)
            }
        }
    }

    pub fn set_title(&self, title: &str) {
        unsafe {
            let title = IdRef::new(NSString::alloc(nil).init_str(title), "title");
            self.window.setTitle_(*title);
        }
    }

    #[inline]
    pub fn show(&self) {
        unsafe { NSWindow::makeKeyAndOrderFront_(*self.window, nil); }
    }

    #[inline]
    pub fn hide(&self) {
        unsafe { NSWindow::orderOut_(*self.window, nil); }
    }

    pub fn get_position(&self) -> Option<(i32, i32)> {
        unsafe {
            let content_rect = NSWindow::contentRectForFrameRect_(*self.window, NSWindow::frame(*self.window));

            // TODO: consider extrapolating the calculations for the y axis to
            // a private method
            Some((content_rect.origin.x as i32, (CGDisplayPixelsHigh(CGMainDisplayID()) as f64 - (content_rect.origin.y + content_rect.size.height)) as i32))
        }
    }

    pub fn set_position(&self, x: i32, y: i32) {
        unsafe {
            let frame = NSWindow::frame(*self.view);

            // NOTE: `setFrameOrigin` might not give desirable results when
            // setting window, as it treats bottom left as origin.
            // `setFrameTopLeftPoint` treats top left as origin (duh), but
            // does not equal the value returned by `get_window_position`
            // (there is a difference by 22 for me on yosemite)

            // TODO: consider extrapolating the calculations for the y axis to
            // a private method
            let dummy = NSRect::new(NSPoint::new(x as f64, CGDisplayPixelsHigh(CGMainDisplayID()) as f64 - (frame.size.height + y as f64)), NSSize::new(0f64, 0f64));
            let conv = NSWindow::frameRectForContentRect_(*self.window, dummy);

            // NSWindow::setFrameTopLeftPoint_(*self.window, conv.origin);
            NSWindow::setFrameOrigin_(*self.window, conv.origin);
        }
    }

    #[inline]
    pub fn get_inner_size(&self) -> Option<(u32, u32)> {
        unsafe {
            let view_frame = NSView::frame(*self.view);
            Some((view_frame.size.width as u32, view_frame.size.height as u32))
        }
    }

    #[inline]
    pub fn get_outer_size(&self) -> Option<(u32, u32)> {
        unsafe {
            let window_frame = NSWindow::frame(*self.window);
            Some((window_frame.size.width as u32, window_frame.size.height as u32))
        }
    }

    #[inline]
    pub fn set_inner_size(&self, width: u32, height: u32) {
        unsafe {
            NSWindow::setContentSize_(*self.window, NSSize::new(width as f64, height as f64));
        }
    }

    #[inline]
    pub fn create_window_proxy(&self) -> WindowProxy {
        WindowProxy
    }

    #[inline]
    pub fn poll_events(&self) -> PollEventsIterator {
        PollEventsIterator {
            window: self
        }
    }

    #[inline]
    pub fn wait_events(&self) -> WaitEventsIterator {
        WaitEventsIterator {
            window: self
        }
    }

    unsafe fn modifier_event(event: id, keymask: appkit::NSEventModifierFlags, key: events::VirtualKeyCode, key_pressed: bool) -> Option<Event> {
        if !key_pressed && NSEvent::modifierFlags(event).contains(keymask) {
            return Some(Event::KeyboardInput(ElementState::Pressed, NSEvent::keyCode(event) as u8, Some(key)));
        } else if key_pressed && !NSEvent::modifierFlags(event).contains(keymask) {
            return Some(Event::KeyboardInput(ElementState::Released, NSEvent::keyCode(event) as u8, Some(key)));
        }

        return None;
    }

    #[inline]
    pub fn platform_display(&self) -> *mut libc::c_void {
        unimplemented!()
    }

    #[inline]
    pub fn platform_window(&self) -> *mut libc::c_void {
        *self.window as *mut libc::c_void
    }

    #[inline]
    pub fn set_window_resize_callback(&mut self, callback: Option<fn(u32, u32)>) {
        self.delegate.state.resize_handler = callback;
    }

    pub fn set_cursor(&self, cursor: MouseCursor) {
        let cursor_name = match cursor {
            MouseCursor::Arrow | MouseCursor::Default => "arrowCursor",
            MouseCursor::Hand => "pointingHandCursor",
            MouseCursor::Grabbing | MouseCursor::Grab => "closedHandCursor",
            MouseCursor::Text => "IBeamCursor",
            MouseCursor::VerticalText => "IBeamCursorForVerticalLayout",
            MouseCursor::Copy => "dragCopyCursor",
            MouseCursor::Alias => "dragLinkCursor",
            MouseCursor::NotAllowed | MouseCursor::NoDrop => "operationNotAllowedCursor",
            MouseCursor::ContextMenu => "contextualMenuCursor",
            MouseCursor::Crosshair => "crosshairCursor",
            MouseCursor::EResize => "resizeRightCursor",
            MouseCursor::NResize => "resizeUpCursor",
            MouseCursor::WResize => "resizeLeftCursor",
            MouseCursor::SResize => "resizeDownCursor",
            MouseCursor::EwResize | MouseCursor::ColResize => "resizeLeftRightCursor",
            MouseCursor::NsResize | MouseCursor::RowResize => "resizeUpDownCursor",

            /// TODO: Find appropriate OSX cursors
            MouseCursor::NeResize | MouseCursor::NwResize |
            MouseCursor::SeResize | MouseCursor::SwResize |
            MouseCursor::NwseResize | MouseCursor::NeswResize |

            MouseCursor::Cell | MouseCursor::NoneCursor |
            MouseCursor::Wait | MouseCursor::Progress | MouseCursor::Help |
            MouseCursor::Move | MouseCursor::AllScroll | MouseCursor::ZoomIn |
            MouseCursor::ZoomOut => "arrowCursor",
        };
        let sel = Sel::register(cursor_name);
        let cls = Class::get("NSCursor").unwrap();
        unsafe {
            use objc::Message;
            let cursor: id = cls.send_message(sel, ()).unwrap();
            let _: () = msg_send![cursor, set];
        }
    }

    pub fn set_cursor_state(&self, state: CursorState) -> Result<(), String> {
        let cls = Class::get("NSCursor").unwrap();

        // TODO: Check for errors.
        match state {
            CursorState::Normal => {
                let _: () = unsafe { msg_send![cls, unhide] };
                let _: i32 = unsafe { CGAssociateMouseAndMouseCursorPosition(true) };
                Ok(())
            },
            CursorState::Hide => {
                let _: () = unsafe { msg_send![cls, hide] };
                Ok(())
            },
            CursorState::Grab => {
                let _: i32 = unsafe { CGAssociateMouseAndMouseCursorPosition(false) };
                Ok(())
            }
        }
    }

    #[inline]
    pub fn hidpi_factor(&self) -> f32 {
        unsafe {
            NSWindow::backingScaleFactor(*self.window) as f32
        }
    }

    #[inline]
    pub fn set_cursor_position(&self, x: i32, y: i32) -> Result<(), ()> {
        let (window_x, window_y) = self.get_position().unwrap_or((0, 0));
        let (cursor_x, cursor_y) = (window_x + x, window_y + y);

        unsafe {
            // TODO: Check for errors.
            let _ = CGWarpMouseCursorPosition(appkit::CGPoint {
                x: cursor_x as appkit::CGFloat,
                y: cursor_y as appkit::CGFloat,
            });
            let _ = CGAssociateMouseAndMouseCursorPosition(true);
        }

        Ok(())
    }
}

impl GlContext for Window {
    #[inline]
    unsafe fn make_current(&self) -> Result<(), ContextError> {
        let _: () = msg_send![*self.context, update];
        self.context.makeCurrentContext();
        Ok(())
    }

    #[inline]
    fn is_current(&self) -> bool {
        unsafe {
            let current = NSOpenGLContext::currentContext(nil);
            if current != nil {
                let is_equal: BOOL = msg_send![current, isEqual:*self.context];
                is_equal != NO
            } else {
                false
            }
        }
    }

    fn get_proc_address(&self, addr: &str) -> *const () {
        let symbol_name: CFString = FromStr::from_str(addr).unwrap();
        let framework_name: CFString = FromStr::from_str("com.apple.opengl").unwrap();
        let framework = unsafe {
            CFBundleGetBundleWithIdentifier(framework_name.as_concrete_TypeRef())
        };
        let symbol = unsafe {
            CFBundleGetFunctionPointerForName(framework, symbol_name.as_concrete_TypeRef())
        };
        symbol as *const _
    }

    #[inline]
    fn swap_buffers(&self) -> Result<(), ContextError> {
        unsafe { 
            let pool = NSAutoreleasePool::new(nil);
            self.context.flushBuffer();
            let _: () = msg_send![pool, release];
        }
        Ok(())
    }

    #[inline]
    fn get_api(&self) -> ::Api {
        ::Api::OpenGl
    }

    #[inline]
    fn get_pixel_format(&self) -> PixelFormat {
        self.pixel_format.clone()
    }
}

struct IdRef(id, &'static str);

impl IdRef {
    fn new(i: id, name: &'static str) -> IdRef {
        println!("new: {:?} {:?}", name, &i);
        IdRef(i, name)
    }

    #[allow(dead_code)]
    fn retain(i: id, name: &'static str) -> IdRef {
        if i != nil {
            let _: id = unsafe { msg_send![i, retain] };
        }
        IdRef(i, name)
    }

    fn non_nil(self) -> Option<IdRef> {
        if self.0 == nil { None } else { Some(self) }
    }
}

impl Drop for IdRef {
    fn drop(&mut self) {
        if self.0 != nil {
            print!("drop: {:?} {:?} ... ", &self.1, &self.0);
            let _: () = unsafe { msg_send![self.0, release] };
            println!("drop finished");
        }
    }
}

impl Deref for IdRef {
    type Target = id;
    fn deref<'a>(&'a self) -> &'a id {
        &self.0
    }
}

impl Clone for IdRef {
    fn clone(&self) -> IdRef {
        if self.0 != nil {
            println!("clone: {:?} {:?}", &self.1, &self.0);
            let _: id = unsafe { msg_send![self.0, retain] };
        } else {
            println!("{:?} {:?} was nil when cloning!", &self.1, &self.0);
        }
        IdRef(self.0, self.1)
    }
}

#[allow(non_snake_case, non_upper_case_globals)]
unsafe fn NSEventToEvent(window: &Window, nsevent: id) -> Option<Event> {
    if nsevent == nil { return None; }

    let event_type = nsevent.eventType();
    appkit::NSApp().sendEvent_(if let appkit::NSKeyDown = event_type { nil } else { nsevent });

    match event_type {
        appkit::NSLeftMouseDown         => { Some(Event::MouseInput(ElementState::Pressed, MouseButton::Left)) },
        appkit::NSLeftMouseUp           => { Some(Event::MouseInput(ElementState::Released, MouseButton::Left)) },
        appkit::NSRightMouseDown        => { Some(Event::MouseInput(ElementState::Pressed, MouseButton::Right)) },
        appkit::NSRightMouseUp          => { Some(Event::MouseInput(ElementState::Released, MouseButton::Right)) },
        appkit::NSMouseMoved            |
        appkit::NSLeftMouseDragged      |
        appkit::NSOtherMouseDragged     |
        appkit::NSRightMouseDragged     => {
            let window_point = nsevent.locationInWindow();
            let cWindow: id = msg_send![nsevent, window];
            let view_point = if cWindow == nil {
                let window_rect = window.window.convertRectFromScreen_(NSRect::new(window_point, NSSize::new(0.0, 0.0)));
                window.view.convertPoint_fromView_(window_rect.origin, nil)
            } else {
                window.view.convertPoint_fromView_(window_point, nil)
            };
            let view_rect = NSView::frame(*window.view);
            let scale_factor = window.hidpi_factor();

            Some(Event::MouseMoved((scale_factor * view_point.x as f32) as i32,
                                   (scale_factor * (view_rect.size.height - view_point.y) as f32) as i32))
        },
        appkit::NSKeyDown => {
            let mut events = VecDeque::new();
            let received_c_str = nsevent.characters().UTF8String();
            let received_str = CStr::from_ptr(received_c_str);
            for received_char in from_utf8(received_str.to_bytes()).unwrap().chars() {
                events.push_back(Event::ReceivedCharacter(received_char));
            }

            let vkey =  event::vkeycode_to_element(NSEvent::keyCode(nsevent));
            events.push_back(Event::KeyboardInput(ElementState::Pressed, NSEvent::keyCode(nsevent) as u8, vkey));
            let event = events.pop_front();
            window.delegate.state.pending_events.lock().unwrap().extend(events.into_iter());
            event
        },
        appkit::NSKeyUp => {
            let vkey =  event::vkeycode_to_element(NSEvent::keyCode(nsevent));

            Some(Event::KeyboardInput(ElementState::Released, NSEvent::keyCode(nsevent) as u8, vkey))
        },
        appkit::NSFlagsChanged => {
            let mut events = VecDeque::new();
            let shift_modifier = Window::modifier_event(nsevent, appkit::NSShiftKeyMask, events::VirtualKeyCode::LShift, shift_pressed);
            if shift_modifier.is_some() {
                shift_pressed = !shift_pressed;
                events.push_back(shift_modifier.unwrap());
            }
            let ctrl_modifier = Window::modifier_event(nsevent, appkit::NSControlKeyMask, events::VirtualKeyCode::LControl, ctrl_pressed);
            if ctrl_modifier.is_some() {
                ctrl_pressed = !ctrl_pressed;
                events.push_back(ctrl_modifier.unwrap());
            }
            let win_modifier = Window::modifier_event(nsevent, appkit::NSCommandKeyMask, events::VirtualKeyCode::LWin, win_pressed);
            if win_modifier.is_some() {
                win_pressed = !win_pressed;
                events.push_back(win_modifier.unwrap());
            }
            let alt_modifier = Window::modifier_event(nsevent, appkit::NSAlternateKeyMask, events::VirtualKeyCode::LAlt, alt_pressed);
            if alt_modifier.is_some() {
                alt_pressed = !alt_pressed;
                events.push_back(alt_modifier.unwrap());
            }
            let event = events.pop_front();
            window.delegate.state.pending_events.lock().unwrap().extend(events.into_iter());
            event
        },
        appkit::NSScrollWheel => {
            use events::MouseScrollDelta::{LineDelta, PixelDelta};
            let scale_factor = window.hidpi_factor();
            let delta = if nsevent.hasPreciseScrollingDeltas() == YES {
                PixelDelta(scale_factor * nsevent.scrollingDeltaX() as f32,
                           scale_factor * nsevent.scrollingDeltaY() as f32)
            } else {
                LineDelta(scale_factor * nsevent.scrollingDeltaX() as f32,
                          scale_factor * nsevent.scrollingDeltaY() as f32)
            };
            let phase = match nsevent.phase() {
                appkit::NSEventPhaseMayBegin | appkit::NSEventPhaseBegan => TouchPhase::Started,
                appkit::NSEventPhaseEnded => TouchPhase::Ended,
                _ => TouchPhase::Moved,
            };
            Some(Event::MouseWheel(delta, phase))
        },
        appkit::NSEventTypePressure => {
            Some(Event::TouchpadPressure(nsevent.pressure(), nsevent.stage()))
        },
        _  => { None },
    }
}
