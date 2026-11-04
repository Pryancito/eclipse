/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Media Source Extensions: `SourceBuffer` (Eclipse OS addition).
//!
//! <https://w3c.github.io/media-source/#sourcebuffer>

use std::cell::Cell;
use std::ops::Range;

use dom_struct::dom_struct;
use js::context::JSContext;
use script_bindings::cell::DomRefCell;
use script_bindings::reflector::reflect_dom_object_with_cx;
use stylo_atoms::Atom;

use crate::dom::bindings::buffer_source::get_buffer_source_copy;
use crate::dom::bindings::codegen::Bindings::MediaSourceBinding::{MediaSourceMethods, ReadyState};
use crate::dom::bindings::codegen::Bindings::SourceBufferBinding::{
    AppendMode, SourceBufferMethods,
};
use crate::dom::bindings::codegen::UnionTypes::ArrayBufferViewOrArrayBuffer;
use crate::dom::bindings::error::{Error, ErrorResult, Fallible};
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::num::Finite;
use crate::dom::bindings::refcounted::Trusted;
use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::bindings::root::{DomRoot, MutNullableDom};
use crate::dom::bindings::str::DOMString;
use crate::dom::eventtarget::EventTarget;
use crate::dom::media::mediasource::{MediaSource, is_type_supported};
use crate::dom::timeranges::{TimeRanges, TimeRangesContainer};
use crate::dom::window::Window;

/// `mp4` or `webm` (audio and video share a demuxer).
fn container_family(type_: &str) -> String {
    let essence = type_
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    essence.split('/').nth(1).unwrap_or("").to_owned()
}

#[dom_struct]
pub(crate) struct SourceBuffer {
    eventtarget: EventTarget,
    /// None once removed from its MediaSource.
    media_source: MutNullableDom<MediaSource>,
    id: u32,
    /// Container of the type the buffer was created with (`video/mp4`, ...).
    container: String,
    updating: Cell<bool>,
    mode: Cell<AppendMode>,
    timestamp_offset: Cell<f64>,
    append_window_start: Cell<f64>,
    append_window_end: Cell<f64>,
    /// Buffered ranges after the last append / remove, in seconds.
    buffered: DomRefCell<Vec<(f64, f64)>>,
    /// Bumped by abort(); a queued append / remove of an older generation
    /// does nothing.
    generation: Cell<u32>,
}

impl SourceBuffer {
    fn new_inherited(media_source: &MediaSource, id: u32, type_: &str) -> SourceBuffer {
        SourceBuffer {
            eventtarget: EventTarget::new_inherited(),
            media_source: MutNullableDom::new(Some(media_source)),
            id,
            container: container_family(type_),
            updating: Cell::new(false),
            mode: Cell::new(AppendMode::Segments),
            timestamp_offset: Cell::new(0.0),
            append_window_start: Cell::new(0.0),
            append_window_end: Cell::new(f64::INFINITY),
            buffered: DomRefCell::new(Vec::new()),
            generation: Cell::new(0),
        }
    }

    pub(crate) fn new(
        cx: &mut JSContext,
        window: &Window,
        media_source: &MediaSource,
        id: u32,
        type_: &str,
    ) -> DomRoot<SourceBuffer> {
        reflect_dom_object_with_cx(
            Box::new(SourceBuffer::new_inherited(media_source, id, type_)),
            window,
            cx,
        )
    }

    pub(crate) fn id(&self) -> u32 {
        self.id
    }

    pub(crate) fn updating(&self) -> bool {
        self.updating.get()
    }

    pub(crate) fn highest_end_time(&self) -> f64 {
        self.buffered
            .borrow()
            .iter()
            .map(|r| r.1)
            .fold(0.0, f64::max)
    }

    fn fire(&self, cx: &mut JSContext, name: &'static str) {
        self.upcast::<EventTarget>()
            .fire_event(cx, Atom::from(name));
    }

    fn queue_event(&self, name: &'static str) {
        let this = Trusted::new(self);
        self.global()
            .task_manager()
            .media_element_task_source()
            .queue(task!(source_buffer_event: move |cx| {
                this.root().fire(cx, name);
            }));
    }

    /// Checks shared by appendBuffer() and remove(); returns the MediaSource.
    fn prepare(&self) -> Fallible<DomRoot<MediaSource>> {
        let Some(media_source) = self.media_source.get() else {
            return Err(Error::InvalidState(None));
        };
        if self.updating.get() || media_source.ready_state() == ReadyState::Closed {
            return Err(Error::InvalidState(None));
        }
        media_source.reopen_if_ended();
        Ok(media_source)
    }

    /// Called when the MediaSource drops this buffer.
    pub(crate) fn abort_for_removal(&self) {
        if self.updating.get() {
            self.generation.set(self.generation.get() + 1);
            self.updating.set(false);
            self.queue_event("abort");
            self.queue_event("updateend");
        }
        self.media_source.set(None);
    }

    fn set_buffered(&self, ranges: Vec<Range<f64>>) {
        *self.buffered.borrow_mut() = ranges.into_iter().map(|r| (r.start, r.end)).collect();
    }

    /// Second half of appendBuffer(): the buffer append algorithm.
    /// <https://w3c.github.io/media-source/#sourcebuffer-buffer-append>
    fn run_append(&self, cx: &mut JSContext, data: Vec<u8>, generation: u32) {
        if generation != self.generation.get() {
            return;
        }
        let Some(media_source) = self.media_source.get() else {
            return;
        };
        let Some(player) = media_source.player() else {
            return;
        };
        let window = self.append_window_start.get()..self.append_window_end.get();
        let result = player.lock().unwrap().append_source_buffer(
            self.id,
            data,
            self.timestamp_offset.get(),
            window,
        );
        self.updating.set(false);
        match result {
            Ok(result) if !result.error => {
                self.set_buffered(result.buffered);
                let end = self.highest_end_time();
                if end > 0.0 {
                    media_source.extend_duration(end);
                }
                self.fire(cx, "update");
                self.fire(cx, "updateend");
            }
            other => {
                // Append error algorithm.
                warn!("MSE: append failed: {other:?}");
                self.fire(cx, "error");
                self.fire(cx, "updateend");
            }
        }
    }

    fn run_remove(&self, cx: &mut JSContext, range: Range<f64>, generation: u32) {
        if generation != self.generation.get() {
            return;
        }
        if let Some(player) = self.media_source.get().and_then(|ms| ms.player()) {
            if let Ok(ranges) = player
                .lock()
                .unwrap()
                .remove_source_buffer_range(self.id, range)
            {
                self.set_buffered(ranges);
            }
        }
        self.updating.set(false);
        self.fire(cx, "update");
        self.fire(cx, "updateend");
    }
}

impl SourceBufferMethods<crate::DomTypeHolder> for SourceBuffer {
    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-mode>
    fn Mode(&self) -> AppendMode {
        self.mode.get()
    }

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-mode>
    fn SetMode(&self, mode: AppendMode) -> ErrorResult {
        if self.media_source.get().is_none() || self.updating.get() {
            return Err(Error::InvalidState(None));
        }
        self.mode.set(mode);
        Ok(())
    }

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-updating>
    fn Updating(&self) -> bool {
        self.updating.get()
    }

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-buffered>
    fn GetBuffered(&self, cx: &mut JSContext) -> Fallible<DomRoot<TimeRanges>> {
        if self.media_source.get().is_none() {
            return Err(Error::InvalidState(None));
        }
        let mut ranges = TimeRangesContainer::default();
        for (start, end) in self.buffered.borrow().iter() {
            let _ = ranges.add(*start, *end);
        }
        Ok(TimeRanges::new(cx, self.global().as_window(), ranges))
    }

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-timestampoffset>
    fn TimestampOffset(&self) -> Finite<f64> {
        Finite::wrap(self.timestamp_offset.get())
    }

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-timestampoffset>
    fn SetTimestampOffset(&self, value: Finite<f64>) -> ErrorResult {
        let value = *value;
        if self.media_source.get().is_none() || self.updating.get() {
            return Err(Error::InvalidState(None));
        }
        if let Some(ms) = self.media_source.get() {
            ms.reopen_if_ended();
        }
        self.timestamp_offset.set(value);
        Ok(())
    }

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-appendwindowstart>
    fn AppendWindowStart(&self) -> Finite<f64> {
        Finite::wrap(self.append_window_start.get())
    }

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-appendwindowstart>
    fn SetAppendWindowStart(&self, value: Finite<f64>) -> ErrorResult {
        let value = *value;
        if self.media_source.get().is_none() || self.updating.get() {
            return Err(Error::InvalidState(None));
        }
        if value < 0.0 || value >= self.append_window_end.get() {
            return Err(Error::Type(c"invalid appendWindowStart".to_owned()));
        }
        self.append_window_start.set(value);
        Ok(())
    }

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-appendwindowend>
    fn AppendWindowEnd(&self) -> f64 {
        self.append_window_end.get()
    }

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-appendwindowend>
    fn SetAppendWindowEnd(&self, value: f64) -> ErrorResult {
        if self.media_source.get().is_none() || self.updating.get() {
            return Err(Error::InvalidState(None));
        }
        if value.is_nan() {
            return Err(Error::Type(c"appendWindowEnd is NaN".to_owned()));
        }
        if value <= self.append_window_start.get() {
            return Err(Error::Type(c"invalid appendWindowEnd".to_owned()));
        }
        self.append_window_end.set(value);
        Ok(())
    }

    event_handler!(updatestart, GetOnupdatestart, SetOnupdatestart);
    event_handler!(update, GetOnupdate, SetOnupdate);
    event_handler!(updateend, GetOnupdateend, SetOnupdateend);
    event_handler!(error, GetOnerror, SetOnerror);
    event_handler!(abort, GetOnabort, SetOnabort);

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-appendbuffer>
    fn AppendBuffer(&self, data: ArrayBufferViewOrArrayBuffer) -> ErrorResult {
        self.prepare()?;
        let bytes = match data {
            ArrayBufferViewOrArrayBuffer::ArrayBufferView(view) => {
                get_buffer_source_copy((&view).into())
            }
            ArrayBufferViewOrArrayBuffer::ArrayBuffer(buffer) => {
                get_buffer_source_copy((&buffer).into())
            }
        };
        self.updating.set(true);
        self.queue_event("updatestart");
        let generation = self.generation.get();
        let this = Trusted::new(self);
        self.global()
            .task_manager()
            .media_element_task_source()
            .queue(task!(source_buffer_append: move |cx| {
                this.root().run_append(cx, bytes, generation);
            }));
        Ok(())
    }

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-abort>
    fn Abort(&self) -> ErrorResult {
        let Some(media_source) = self.media_source.get() else {
            return Err(Error::InvalidState(None));
        };
        if media_source.ready_state() != ReadyState::Open {
            return Err(Error::InvalidState(None));
        }
        if self.updating.get() {
            self.generation.set(self.generation.get() + 1);
            self.updating.set(false);
            self.queue_event("abort");
            self.queue_event("updateend");
        }
        if let Some(player) = media_source.player() {
            let _ = player.lock().unwrap().reset_source_buffer_parser(self.id);
        }
        self.append_window_start.set(0.0);
        self.append_window_end.set(f64::INFINITY);
        Ok(())
    }

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-changetype>
    fn ChangeType(&self, type_: DOMString) -> ErrorResult {
        let type_ = type_.str().to_string();
        if type_.is_empty() {
            return Err(Error::Type(c"empty type".to_owned()));
        }
        if self.media_source.get().is_none() || self.updating.get() {
            return Err(Error::InvalidState(None));
        }
        if !is_type_supported(&type_) {
            return Err(Error::NotSupported(None));
        }
        // The same container keeps working with the current demuxer (a new
        // init segment brings the new codec). Another container would need a
        // different demuxer, which we do not support yet.
        if container_family(&type_) == self.container {
            Ok(())
        } else {
            Err(Error::NotSupported(None))
        }
    }

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-remove>
    fn Remove(&self, start: Finite<f64>, end: f64) -> ErrorResult {
        let start = *start;
        let media_source = self.prepare()?;
        let duration = media_source.Duration();
        if duration.is_nan() || start < 0.0 || start > duration {
            return Err(Error::Type(c"invalid start".to_owned()));
        }
        if end.is_nan() || end <= start {
            return Err(Error::Type(c"invalid end".to_owned()));
        }
        self.updating.set(true);
        self.queue_event("updatestart");
        let generation = self.generation.get();
        let this = Trusted::new(self);
        self.global()
            .task_manager()
            .media_element_task_source()
            .queue(task!(source_buffer_remove: move |cx| {
                this.root().run_remove(cx, start..end, generation);
            }));
        Ok(())
    }
}
