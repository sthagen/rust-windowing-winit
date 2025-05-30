use std::cell::OnceCell;
use std::time::Duration;

use js_sys::{Array, Function, Object, Promise};
use wasm_bindgen::closure::Closure;
use wasm_bindgen::prelude::wasm_bindgen;
use wasm_bindgen::{JsCast, JsValue};
use web_sys::{
    AbortController, AbortSignal, Blob, BlobPropertyBag, MessageChannel, MessagePort, Url, Worker,
};

use crate::{PollStrategy, WaitUntilStrategy};

#[derive(Debug)]
pub struct Schedule {
    _closure: Closure<dyn FnMut()>,
    inner: Inner,
}

#[derive(Debug)]
enum Inner {
    Scheduler {
        controller: AbortController,
    },
    IdleCallback {
        window: web_sys::Window,
        handle: u32,
    },
    Timeout {
        window: web_sys::Window,
        handle: i32,
        port: MessagePort,
        _timeout_closure: Closure<dyn FnMut()>,
    },
    Worker(MessagePort),
}

impl Schedule {
    pub fn new<F>(strategy: PollStrategy, window: &web_sys::Window, f: F) -> Schedule
    where
        F: 'static + FnMut(),
    {
        if strategy == PollStrategy::Scheduler && has_scheduler_support(window) {
            Self::new_scheduler(window, f, None)
        } else if strategy == PollStrategy::IdleCallback && has_idle_callback_support(window) {
            Self::new_idle_callback(window.clone(), f)
        } else {
            Self::new_timeout(window.clone(), f, None)
        }
    }

    pub fn new_with_duration<F>(
        strategy: WaitUntilStrategy,
        window: &web_sys::Window,
        f: F,
        duration: Duration,
    ) -> Schedule
    where
        F: 'static + FnMut(),
    {
        match strategy {
            WaitUntilStrategy::Scheduler => {
                if has_scheduler_support(window) {
                    Self::new_scheduler(window, f, Some(duration))
                } else {
                    Self::new_timeout(window.clone(), f, Some(duration))
                }
            },
            WaitUntilStrategy::Worker => Self::new_worker(f, duration),
        }
    }

    fn new_scheduler<F>(window: &web_sys::Window, f: F, duration: Option<Duration>) -> Schedule
    where
        F: 'static + FnMut(),
    {
        let window: &WindowSupportExt = window.unchecked_ref();
        let scheduler = window.scheduler();

        let closure = Closure::new(f);
        let options: SchedulerPostTaskOptions = Object::new().unchecked_into();
        let controller = AbortController::new().expect("Failed to create `AbortController`");
        options.set_signal(&controller.signal());

        if let Some(duration) = duration {
            // `Duration::as_millis()` always rounds down (because of truncation), we want to round
            // up instead. This makes sure that the we never wake up **before** the given time.
            let duration = duration
                .as_secs()
                .checked_mul(1000)
                .and_then(|secs| secs.checked_add(duration.subsec_micros().div_ceil(1000).into()))
                .unwrap_or(u64::MAX);

            options.set_delay(duration as f64);
        }

        thread_local! {
            static REJECT_HANDLER: Closure<dyn FnMut(JsValue)> = Closure::new(|_| ());
        }
        REJECT_HANDLER.with(|handler| {
            let _ = scheduler
                .post_task_with_options(closure.as_ref().unchecked_ref(), &options)
                .catch(handler);
        });

        Schedule { _closure: closure, inner: Inner::Scheduler { controller } }
    }

    fn new_idle_callback<F>(window: web_sys::Window, f: F) -> Schedule
    where
        F: 'static + FnMut(),
    {
        let closure = Closure::new(f);
        let handle = window
            .request_idle_callback(closure.as_ref().unchecked_ref())
            .expect("Failed to request idle callback");

        Schedule { _closure: closure, inner: Inner::IdleCallback { window, handle } }
    }

    fn new_timeout<F>(window: web_sys::Window, f: F, duration: Option<Duration>) -> Schedule
    where
        F: 'static + FnMut(),
    {
        let channel = MessageChannel::new().unwrap();
        let closure = Closure::new(f);
        let port_1 = channel.port1();
        port_1.set_onmessage(Some(closure.as_ref().unchecked_ref()));
        port_1.start();

        let port_2 = channel.port2();
        let timeout_closure = Closure::new(move || {
            port_2.post_message(&JsValue::UNDEFINED).expect("Failed to send message")
        });
        let handle = if let Some(duration) = duration {
            // `Duration::as_millis()` always rounds down (because of truncation), we want to round
            // up instead. This makes sure that the we never wake up **before** the given time.
            let duration = duration
                .as_secs()
                .try_into()
                .ok()
                .and_then(|secs: i32| secs.checked_mul(1000))
                .and_then(|secs: i32| {
                    let millis: i32 = duration
                        .subsec_micros()
                        .div_ceil(1000)
                        .try_into()
                        .expect("millis are somehow bigger then 1K");
                    secs.checked_add(millis)
                })
                .unwrap_or(i32::MAX);

            window.set_timeout_with_callback_and_timeout_and_arguments_0(
                timeout_closure.as_ref().unchecked_ref(),
                duration,
            )
        } else {
            window.set_timeout_with_callback(timeout_closure.as_ref().unchecked_ref())
        }
        .expect("Failed to set timeout");

        Schedule {
            _closure: closure,
            inner: Inner::Timeout {
                window,
                handle,
                port: port_1,
                _timeout_closure: timeout_closure,
            },
        }
    }

    fn new_worker<F>(f: F, duration: Duration) -> Schedule
    where
        F: 'static + FnMut(),
    {
        thread_local! {
            static URL: ScriptUrl = ScriptUrl::new(include_str!("../script/worker.min.js"));
            static WORKER: Worker = URL.with(|url| Worker::new(&url.0)).expect("`new Worker()` is not expected to fail with a local script");
        }

        let channel = MessageChannel::new().unwrap();
        let closure = Closure::new(f);
        let port_1 = channel.port1();
        port_1.set_onmessage(Some(closure.as_ref().unchecked_ref()));
        port_1.start();

        // `Duration::as_millis()` always rounds down (because of truncation), we want to round
        // up instead. This makes sure that the we never wake up **before** the given time.
        let duration = duration
            .as_secs()
            .try_into()
            .ok()
            .and_then(|secs: u32| secs.checked_mul(1000))
            .and_then(|secs| secs.checked_add(duration.subsec_micros().div_ceil(1000)))
            .unwrap_or(u32::MAX);

        WORKER
            .with(|worker| {
                let port_2 = channel.port2();
                worker.post_message_with_transfer(
                    &Array::of2(&port_2, &duration.into()),
                    &Array::of1(&port_2).into(),
                )
            })
            .expect("`Worker.postMessage()` is not expected to fail");

        Schedule { _closure: closure, inner: Inner::Worker(port_1) }
    }
}

impl Drop for Schedule {
    fn drop(&mut self) {
        match &self.inner {
            Inner::Scheduler { controller, .. } => controller.abort(),
            Inner::IdleCallback { window, handle, .. } => window.cancel_idle_callback(*handle),
            Inner::Timeout { window, handle, port, .. } => {
                window.clear_timeout_with_handle(*handle);
                port.close();
                port.set_onmessage(None);
            },
            Inner::Worker(port) => {
                port.close();
                port.set_onmessage(None);
            },
        }
    }
}

fn has_scheduler_support(window: &web_sys::Window) -> bool {
    thread_local! {
        static SCHEDULER_SUPPORT: OnceCell<bool> = const { OnceCell::new() };
    }

    SCHEDULER_SUPPORT.with(|support| {
        *support.get_or_init(|| {
            #[wasm_bindgen]
            extern "C" {
                type SchedulerSupport;

                #[wasm_bindgen(method, getter, js_name = scheduler)]
                fn has_scheduler(this: &SchedulerSupport) -> JsValue;
            }

            let support: &SchedulerSupport = window.unchecked_ref();

            !support.has_scheduler().is_undefined()
        })
    })
}

fn has_idle_callback_support(window: &web_sys::Window) -> bool {
    thread_local! {
        static IDLE_CALLBACK_SUPPORT: OnceCell<bool> = const { OnceCell::new() };
    }

    IDLE_CALLBACK_SUPPORT.with(|support| {
        *support.get_or_init(|| {
            #[wasm_bindgen]
            extern "C" {
                type IdleCallbackSupport;

                #[wasm_bindgen(method, getter, js_name = requestIdleCallback)]
                fn has_request_idle_callback(this: &IdleCallbackSupport) -> JsValue;
            }

            let support: &IdleCallbackSupport = window.unchecked_ref();
            !support.has_request_idle_callback().is_undefined()
        })
    })
}

struct ScriptUrl(String);

impl ScriptUrl {
    fn new(script: &str) -> Self {
        let sequence = Array::of1(&script.into());
        let property = BlobPropertyBag::new();
        property.set_type("text/javascript");
        let blob = Blob::new_with_str_sequence_and_options(&sequence, &property)
            .expect("`new Blob()` should never throw");

        let url = Url::create_object_url_with_blob(&blob)
            .expect("`URL.createObjectURL()` should never throw");

        Self(url)
    }
}

impl Drop for ScriptUrl {
    fn drop(&mut self) {
        Url::revoke_object_url(&self.0).expect("`URL.revokeObjectURL()` should never throw");
    }
}

#[wasm_bindgen]
extern "C" {
    type WindowSupportExt;

    #[wasm_bindgen(method, getter)]
    fn scheduler(this: &WindowSupportExt) -> Scheduler;

    type Scheduler;

    #[wasm_bindgen(method, js_name = postTask)]
    fn post_task_with_options(
        this: &Scheduler,
        callback: &Function,
        options: &SchedulerPostTaskOptions,
    ) -> Promise;

    type SchedulerPostTaskOptions;

    #[wasm_bindgen(method, setter, js_name = delay)]
    fn set_delay(this: &SchedulerPostTaskOptions, value: f64);

    #[wasm_bindgen(method, setter, js_name = signal)]
    fn set_signal(this: &SchedulerPostTaskOptions, value: &AbortSignal);
}
