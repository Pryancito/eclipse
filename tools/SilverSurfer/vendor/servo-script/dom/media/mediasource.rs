/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Media Source Extensions: `MediaSource` (Eclipse OS addition).
//!
//! <https://w3c.github.io/media-source/#mediasource>
//!
//! The demuxing and the playback of appended data live in the media
//! backend (servo-media-gstreamer `mse.rs`); this object keeps the
//! JavaScript-visible state and talks to the player of the media element it
//! is attached to.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use dom_struct::dom_struct;
use js::rust::HandleObject;
use script_bindings::reflector::reflect_dom_object_with_proto;
use servo_media::player::Player;
use servo_media::{ServoMedia, SupportsMediaType};
use stylo_atoms::Atom;
use uuid::Uuid;

use crate::dom::bindings::codegen::Bindings::MediaSourceBinding::{
    EndOfStreamError, MediaSourceMethods, ReadyState,
};
use crate::dom::bindings::error::{Error, ErrorResult, Fallible};
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::num::Finite;
use crate::dom::bindings::refcounted::Trusted;
use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::bindings::root::{DomRoot, MutNullableDom};
use crate::dom::bindings::str::DOMString;
use crate::dom::eventtarget::EventTarget;
use crate::dom::globalscope::GlobalScope;
use crate::dom::html::htmlmediaelement::HTMLMediaElement;
use crate::dom::media::sourcebuffer::SourceBuffer;
use crate::dom::media::sourcebufferlist::SourceBufferList;
use crate::dom::window::Window;

thread_local! {
    /// MediaSource object URLs of this script thread (`URL.createObjectURL`).
    static OBJECT_URLS: RefCell<HashMap<String, Trusted<MediaSource>>> =
        RefCell::new(HashMap::new());
}

/// Container types we can demux, and the codecs the backend can decode.
pub(crate) fn is_type_supported(type_: &str) -> bool {
    let essence = type_
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if !matches!(
        essence.as_str(),
        "video/mp4" | "audio/mp4" | "video/webm" | "audio/webm"
    ) {
        return false;
    }
    ServoMedia::get().can_play_type(type_) != SupportsMediaType::No
}

#[dom_struct]
pub(crate) struct MediaSource {
    eventtarget: EventTarget,
    source_buffers: MutNullableDom<SourceBufferList>,
    active_source_buffers: MutNullableDom<SourceBufferList>,
    media_element: MutNullableDom<HTMLMediaElement>,
    ready_state: Cell<ReadyState>,
    duration: Cell<f64>,
    next_buffer_id: Cell<u32>,
}

impl MediaSource {
    fn new_inherited() -> MediaSource {
        MediaSource {
            eventtarget: EventTarget::new_inherited(),
            source_buffers: Default::default(),
            active_source_buffers: Default::default(),
            media_element: Default::default(),
            ready_state: Cell::new(ReadyState::Closed),
            duration: Cell::new(f64::NAN),
            next_buffer_id: Cell::new(0),
        }
    }

    fn new_with_proto(
        cx: &mut js::context::JSContext,
        global: &GlobalScope,
        proto: Option<HandleObject>,
    ) -> DomRoot<MediaSource> {
        let this = reflect_dom_object_with_proto(
            cx,
            Box::new(MediaSource::new_inherited()),
            global,
            proto,
        );
        let window = global.as_window();
        this.source_buffers
            .set(Some(&SourceBufferList::new(cx, window)));
        this.active_source_buffers
            .set(Some(&SourceBufferList::new(cx, window)));
        this
    }

    /// `URL.createObjectURL(mediaSource)`.
    pub(crate) fn create_object_url(&self) -> DOMString {
        let global = self.global();
        let origin = global.origin();
        let url = format!(
            "blob:{}/{}",
            origin.immutable().ascii_serialization(),
            Uuid::new_v4()
        );
        OBJECT_URLS.with(|urls| {
            urls.borrow_mut().insert(url.clone(), Trusted::new(self));
        });
        DOMString::from(url)
    }

    /// `URL.revokeObjectURL(url)`; true if it named a MediaSource.
    pub(crate) fn revoke_object_url(url: &str) -> bool {
        OBJECT_URLS.with(|urls| urls.borrow_mut().remove(url).is_some())
    }

    /// The MediaSource behind an object URL, if any.
    pub(crate) fn from_object_url(url: &str) -> Option<DomRoot<MediaSource>> {
        OBJECT_URLS.with(|urls| urls.borrow().get(url).map(|t| t.root()))
    }

    pub(crate) fn player(&self) -> Option<Arc<Mutex<dyn Player>>> {
        self.media_element
            .get()
            .and_then(|element| element.player())
    }

    pub(crate) fn ready_state(&self) -> ReadyState {
        self.ready_state.get()
    }

    pub(crate) fn source_buffers(&self) -> DomRoot<SourceBufferList> {
        self.source_buffers.get().unwrap()
    }

    pub(crate) fn queue_event(&self, name: &'static str) {
        let this = Trusted::new(self);
        self.global()
            .task_manager()
            .media_element_task_source()
            .queue(task!(media_source_event: move |cx| {
                this.root().upcast::<EventTarget>().fire_event(cx, Atom::from(name));
            }));
    }

    /// <https://w3c.github.io/media-source/#mediasource-attach>
    /// The media element calls this from its resource fetch algorithm.
    pub(crate) fn attach(&self, element: &HTMLMediaElement) -> bool {
        if self.ready_state.get() != ReadyState::Closed {
            return false;
        }
        self.media_element.set(Some(element));
        self.ready_state.set(ReadyState::Open);
        self.queue_event("sourceopen");
        true
    }

    /// <https://w3c.github.io/media-source/#mediasource-detach>
    pub(crate) fn detach(&self) {
        if self.ready_state.get() == ReadyState::Closed {
            return;
        }
        self.ready_state.set(ReadyState::Closed);
        self.duration.set(f64::NAN);
        self.media_element.set(None);
        self.active_source_buffers().clear();
        self.source_buffers().clear();
        self.queue_event("sourceclose");
    }

    fn active_source_buffers(&self) -> DomRoot<SourceBufferList> {
        self.active_source_buffers.get().unwrap()
    }

    /// An append or remove reopens an ended MediaSource.
    pub(crate) fn reopen_if_ended(&self) {
        if self.ready_state.get() == ReadyState::Ended {
            self.ready_state.set(ReadyState::Open);
            if let Some(player) = self.player() {
                let _ = player.lock().unwrap().set_media_source_ended(false);
            }
            self.queue_event("sourceopen");
        }
    }

    /// <https://w3c.github.io/media-source/#duration-change-algorithm>
    fn change_duration(&self, duration: f64) {
        if self.duration.get() == duration {
            return;
        }
        self.duration.set(duration);
        if let Some(player) = self.player() {
            let _ = player.lock().unwrap().set_media_source_duration(duration);
        }
        if let Some(element) = self.media_element.get() {
            element.media_source_duration_changed(duration);
        }
    }

    /// Grows the duration to cover what was appended
    /// (<https://w3c.github.io/media-source/#sourcebuffer-coded-frame-processing>, step 5).
    pub(crate) fn extend_duration(&self, end: f64) {
        let duration = self.duration.get();
        if duration.is_nan() || end > duration {
            self.change_duration(end);
        }
    }

    fn any_updating(&self) -> bool {
        self.source_buffers().any_updating()
    }
}

impl MediaSourceMethods<crate::DomTypeHolder> for MediaSource {
    /// <https://w3c.github.io/media-source/#dom-mediasource-constructor>
    fn Constructor(
        cx: &mut js::context::JSContext,
        global: &Window,
        proto: Option<HandleObject>,
    ) -> Fallible<DomRoot<MediaSource>> {
        Ok(MediaSource::new_with_proto(cx, &global.global(), proto))
    }

    /// <https://w3c.github.io/media-source/#dom-mediasource-sourcebuffers>
    fn SourceBuffers(&self) -> DomRoot<SourceBufferList> {
        self.source_buffers()
    }

    /// <https://w3c.github.io/media-source/#dom-mediasource-activesourcebuffers>
    fn ActiveSourceBuffers(&self) -> DomRoot<SourceBufferList> {
        self.active_source_buffers()
    }

    /// <https://w3c.github.io/media-source/#dom-mediasource-readystate>
    fn ReadyState(&self) -> ReadyState {
        self.ready_state.get()
    }

    /// <https://w3c.github.io/media-source/#dom-mediasource-duration>
    fn Duration(&self) -> f64 {
        if self.ready_state.get() == ReadyState::Closed {
            return f64::NAN;
        }
        self.duration.get()
    }

    /// <https://w3c.github.io/media-source/#dom-mediasource-duration>
    fn SetDuration(&self, value: f64) -> ErrorResult {
        if value.is_nan() || value < 0.0 {
            return Err(Error::Type(
                c"duration must be a positive number".to_owned(),
            ));
        }
        if self.ready_state.get() != ReadyState::Open {
            return Err(Error::InvalidState(None));
        }
        if self.any_updating() {
            return Err(Error::InvalidState(None));
        }
        self.change_duration(value);
        Ok(())
    }

    // https://w3c.github.io/media-source/#dom-mediasource-onsourceopen
    event_handler!(sourceopen, GetOnsourceopen, SetOnsourceopen);
    // https://w3c.github.io/media-source/#dom-mediasource-onsourceended
    event_handler!(sourceended, GetOnsourceended, SetOnsourceended);
    // https://w3c.github.io/media-source/#dom-mediasource-onsourceclose
    event_handler!(sourceclose, GetOnsourceclose, SetOnsourceclose);

    /// <https://w3c.github.io/media-source/#dom-mediasource-canconstructindedicatedworker>
    fn CanConstructInDedicatedWorker(_: &Window) -> bool {
        false
    }

    /// <https://w3c.github.io/media-source/#dom-mediasource-addsourcebuffer>
    fn AddSourceBuffer(
        &self,
        cx: &mut js::context::JSContext,
        type_: DOMString,
    ) -> Fallible<DomRoot<SourceBuffer>> {
        let type_ = type_.str().to_string();
        if type_.is_empty() {
            return Err(Error::Type(c"empty type".to_owned()));
        }
        if !is_type_supported(&type_) {
            return Err(Error::NotSupported(None));
        }
        if self.ready_state.get() != ReadyState::Open {
            return Err(Error::InvalidState(None));
        }
        let Some(player) = self.player() else {
            return Err(Error::InvalidState(None));
        };
        let id = self.next_buffer_id.get();
        self.next_buffer_id.set(id + 1);
        if let Err(error) = player.lock().unwrap().add_source_buffer(id, &type_) {
            warn!("MSE: addSourceBuffer({type_}) failed: {error:?}");
            return Err(Error::NotSupported(None));
        }
        let buffer = SourceBuffer::new(cx, self.global().as_window(), self, id, &type_);
        self.source_buffers().add(&buffer);
        self.active_source_buffers().add(&buffer);
        Ok(buffer)
    }

    /// <https://w3c.github.io/media-source/#dom-mediasource-removesourcebuffer>
    fn RemoveSourceBuffer(&self, buffer: &SourceBuffer) -> ErrorResult {
        if !self.source_buffers().contains(buffer) {
            return Err(Error::NotFound(None));
        }
        buffer.abort_for_removal();
        if let Some(player) = self.player() {
            let _ = player.lock().unwrap().remove_source_buffer(buffer.id());
        }
        self.active_source_buffers().remove(buffer);
        self.source_buffers().remove(buffer);
        Ok(())
    }

    /// <https://w3c.github.io/media-source/#dom-mediasource-endofstream>
    fn EndOfStream(&self, error: Option<EndOfStreamError>) -> ErrorResult {
        if self.ready_state.get() != ReadyState::Open || self.any_updating() {
            return Err(Error::InvalidState(None));
        }
        self.ready_state.set(ReadyState::Ended);
        self.queue_event("sourceended");
        if error.is_none() {
            // Set the duration to the highest end time of the buffered data.
            let end = self.source_buffers().highest_end_time();
            if end > 0.0 {
                self.change_duration(end);
            }
            if let Some(player) = self.player() {
                let _ = player.lock().unwrap().set_media_source_ended(true);
            }
        }
        Ok(())
    }

    /// <https://w3c.github.io/media-source/#dom-mediasource-setliveseekablerange>
    fn SetLiveSeekableRange(&self, start: Finite<f64>, end: Finite<f64>) -> ErrorResult {
        let (start, end) = (*start, *end);
        if self.ready_state.get() != ReadyState::Open {
            return Err(Error::InvalidState(None));
        }
        if start < 0.0 || start > end {
            return Err(Error::Type(c"invalid range".to_owned()));
        }
        Ok(())
    }

    /// <https://w3c.github.io/media-source/#dom-mediasource-clearliveseekablerange>
    fn ClearLiveSeekableRange(&self) -> ErrorResult {
        if self.ready_state.get() != ReadyState::Open {
            return Err(Error::InvalidState(None));
        }
        Ok(())
    }

    /// <https://w3c.github.io/media-source/#dom-mediasource-istypesupported>
    fn IsTypeSupported(_: &Window, type_: DOMString) -> bool {
        is_type_supported(&type_.str())
    }
}
