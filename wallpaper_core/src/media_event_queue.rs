//! Asynchronous bridge from IMFMediaEventGenerator callbacks to the native
//! render thread. Callback threads never decode, render, or block.

use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    mpsc, Arc,
};

use windows::{
    core::{implement, IUnknown, Interface},
    Win32::{
        Foundation::E_NOTIMPL,
        Media::MediaFoundation::{
            IMFAsyncCallback, IMFAsyncCallback_Impl, IMFAsyncResult, IMFMediaEvent,
            IMFMediaEventGenerator,
        },
    },
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EventOrigin {
    Transform,
    Source(u64),
    Stream(u64),
}

pub struct QueuedMediaEvent {
    pub origin: EventOrigin,
    pub event: Result<IMFMediaEvent, String>,
}

#[implement(IMFAsyncCallback)]
struct AsyncMediaEventCallback {
    generator: IMFMediaEventGenerator,
    origin: EventOrigin,
    sender: mpsc::Sender<QueuedMediaEvent>,
    active: Arc<AtomicBool>,
    engine_stop: Arc<AtomicBool>,
    callback_count: Arc<AtomicU64>,
}

impl IMFAsyncCallback_Impl for AsyncMediaEventCallback_Impl {
    fn GetParameters(&self, _flags: *mut u32, _queue: *mut u32) -> windows::core::Result<()> {
        // E_NOTIMPL asks Media Foundation to select its standard callback queue.
        Err(E_NOTIMPL.into())
    }

    fn Invoke(&self, result: Option<&IMFAsyncResult>) -> windows::core::Result<()> {
        self.callback_count.fetch_add(1, Ordering::Release);
        let Some(result) = result else {
            let _ = self.sender.send(QueuedMediaEvent {
                origin: self.origin,
                event: Err("Media Foundation invoked an event callback without a result.".into()),
            });
            return Ok(());
        };

        let event = unsafe { self.generator.EndGetEvent(result) };
        let event = event
            .and_then(|event| unsafe {
                event.GetStatus()?.ok()?;
                Ok(event)
            })
            .map_err(|error| format!("Asynchronous Media Foundation event failed: {error}"));
        let delivered = self
            .sender
            .send(QueuedMediaEvent {
                origin: self.origin,
                event,
            })
            .is_ok();

        if delivered
            && self.active.load(Ordering::Acquire)
            && !self.engine_stop.load(Ordering::Acquire)
        {
            // The callback itself was supplied as punkState. Recover that COM
            // identity and use it for the one legal next BeginGetEvent call.
            let state = unsafe { result.GetState()? };
            let callback: IMFAsyncCallback = state.cast()?;
            unsafe {
                self.generator.BeginGetEvent(&callback, &state)?;
            }
        }
        Ok(())
    }
}

pub struct MediaEventSubscription {
    active: Arc<AtomicBool>,
    _callback: IMFAsyncCallback,
}

impl MediaEventSubscription {
    pub unsafe fn start(
        generator: &IMFMediaEventGenerator,
        origin: EventOrigin,
        sender: mpsc::Sender<QueuedMediaEvent>,
        engine_stop: Arc<AtomicBool>,
        callback_count: Arc<AtomicU64>,
    ) -> Result<Self, String> {
        let active = Arc::new(AtomicBool::new(true));
        let callback: IMFAsyncCallback = AsyncMediaEventCallback {
            generator: generator.clone(),
            origin,
            sender,
            active: Arc::clone(&active),
            engine_stop,
            callback_count,
        }
        .into();
        let state: IUnknown = callback.cast().map_err(|error| {
            format!("Could not create Media Foundation callback state: {error}")
        })?;
        generator
            .BeginGetEvent(&callback, &state)
            .map_err(|error| format!("Could not subscribe to Media Foundation events: {error}"))?;
        Ok(Self {
            active,
            _callback: callback,
        })
    }
}

impl Drop for MediaEventSubscription {
    fn drop(&mut self) {
        self.active.store(false, Ordering::Release);
    }
}
