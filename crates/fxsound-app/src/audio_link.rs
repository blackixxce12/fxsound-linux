//! The controller's end of the audio engine: the real [`EngineHandle`], or a stand-in that records
//! what it is told and says what a test tells it to.
//!
//! The controller talks to the engine through exactly the calls [`EngineHandle`] offers — control
//! messages, per-lane events, the two parameter snapshots, per-lane meters and the notifications
//! coming back — so the stand-in offers the same ones and nothing else. That is what lets a test
//! drive the whole controller with a fake message feed and assert on what reached the engine,
//! rather than on the controller's own copies of it, without a PipeWire server anywhere.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use fxsound_audio::EngineHandle;
use fxsound_core::DeviceDirection;
use fxsound_core::messages::{AudioToUi, DspEvent, DspParams, InputDspParams, Meters, UiToAudio};

/// What the controller holds while there is an engine to talk to.
pub(crate) enum AudioLink {
    /// The audio thread.
    Engine(EngineHandle),
    /// A recording stand-in ([`FakeEngine`]).
    Fake(FakeEngine),
}

impl AudioLink {
    /// A control-plane request.
    pub(crate) fn send(&self, message: UiToAudio) {
        match self {
            Self::Engine(engine) => engine.send(message),
            Self::Fake(fake) => fake.record().sent.push(message),
        }
    }

    /// A one-shot event for one lane's chain.
    pub(crate) fn send_event(&self, direction: DeviceDirection, event: DspEvent) {
        match self {
            Self::Engine(engine) => engine.send_event(direction, event),
            Self::Fake(fake) => fake.record().events.push((direction, event)),
        }
    }

    /// The output lane's snapshot.
    pub(crate) fn set_params(&mut self, params: DspParams) {
        match self {
            Self::Engine(engine) => engine.set_params(params),
            Self::Fake(fake) => {
                // What the audio thread would read: the engine sanitises on the way in.
                let mut params = params;
                params.sanitise();
                fake.record().params.push(params);
            }
        }
    }

    /// The input lane's snapshot.
    pub(crate) fn set_input_params(&mut self, params: InputDspParams) {
        match self {
            Self::Engine(engine) => engine.set_input_params(params),
            Self::Fake(fake) => {
                let mut params = params;
                params.sanitise();
                fake.record().input_params.push(params);
            }
        }
    }

    /// The latest meters one lane published.
    pub(crate) fn meters(&mut self, direction: DeviceDirection) -> Meters {
        match self {
            Self::Engine(engine) => engine.meters(direction),
            Self::Fake(fake) => fake.record().meters[lane(direction)],
        }
    }

    /// The next notification, if any.
    pub(crate) fn try_recv(&self) -> Option<AudioToUi> {
        match self {
            Self::Engine(engine) => engine.try_recv(),
            Self::Fake(fake) => fake.record().feed.pop_front(),
        }
    }

    /// Stop the engine; see [`EngineHandle::shutdown`].
    pub(crate) fn shutdown(self) {
        match self {
            Self::Engine(engine) => engine.shutdown(),
            Self::Fake(fake) => fake.record().shut_down = true,
        }
    }
}

/// A stand-in audio engine for tests: it records every request the controller makes and answers
/// with whatever the test queued, in order, the next time the controller polls.
///
/// Cloning it clones the handle, not the record, so a test keeps one clone and hands the other to
/// the controller.
#[doc(hidden)]
#[derive(Debug, Clone, Default)]
pub struct FakeEngine {
    record: Arc<Mutex<Record>>,
}

#[derive(Debug, Default)]
struct Record {
    feed: VecDeque<AudioToUi>,
    sent: Vec<UiToAudio>,
    events: Vec<(DeviceDirection, DspEvent)>,
    params: Vec<DspParams>,
    input_params: Vec<InputDspParams>,
    meters: [Meters; 2],
    shut_down: bool,
}

impl FakeEngine {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn record(&self) -> MutexGuard<'_, Record> {
        // A test that panicked while holding the lock has already failed; what it left is still
        // worth reading.
        self.record.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Queue a notification for the controller's next poll.
    pub fn feed(&self, message: AudioToUi) {
        self.record().feed.push_back(message);
    }

    /// Everything the controller asked of the engine since the last take, in order.
    #[must_use]
    pub fn take_sent(&self) -> Vec<UiToAudio> {
        std::mem::take(&mut self.record().sent)
    }

    /// Every per-lane event since the last take, in order.
    #[must_use]
    pub fn take_events(&self) -> Vec<(DeviceDirection, DspEvent)> {
        std::mem::take(&mut self.record().events)
    }

    /// The output snapshot the engine would be running, if one was ever published.
    #[must_use]
    pub fn params(&self) -> Option<DspParams> {
        self.record().params.last().copied()
    }

    /// The input snapshot the engine would be running, if one was ever published.
    #[must_use]
    pub fn input_params(&self) -> Option<InputDspParams> {
        self.record().input_params.last().copied()
    }

    /// How many snapshots of each lane were published: output, input.
    #[must_use]
    pub fn publications(&self) -> (usize, usize) {
        let record = self.record();
        (record.params.len(), record.input_params.len())
    }

    /// What one lane's chain measures from now on.
    pub fn set_meters(&self, direction: DeviceDirection, meters: Meters) {
        self.record().meters[lane(direction)] = meters;
    }

    /// Whether the controller has shut the engine down.
    #[must_use]
    pub fn is_shut_down(&self) -> bool {
        self.record().shut_down
    }
}

const fn lane(direction: DeviceDirection) -> usize {
    match direction {
        DeviceDirection::Output => 0,
        DeviceDirection::Input => 1,
    }
}
