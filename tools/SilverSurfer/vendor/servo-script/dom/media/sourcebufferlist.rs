/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Media Source Extensions: `SourceBufferList` (Eclipse OS addition).
//!
//! <https://w3c.github.io/media-source/#sourcebufferlist>

use dom_struct::dom_struct;
use js::context::JSContext;
use script_bindings::cell::DomRefCell;
use script_bindings::reflector::reflect_dom_object_with_cx;
use stylo_atoms::Atom;

use crate::dom::bindings::codegen::Bindings::SourceBufferListBinding::SourceBufferListMethods;
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::refcounted::Trusted;
use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::bindings::root::{Dom, DomRoot};
use crate::dom::eventtarget::EventTarget;
use crate::dom::media::sourcebuffer::SourceBuffer;
use crate::dom::window::Window;

#[dom_struct]
pub(crate) struct SourceBufferList {
    eventtarget: EventTarget,
    buffers: DomRefCell<Vec<Dom<SourceBuffer>>>,
}

impl SourceBufferList {
    pub(crate) fn new(cx: &mut JSContext, window: &Window) -> DomRoot<SourceBufferList> {
        reflect_dom_object_with_cx(
            Box::new(SourceBufferList {
                eventtarget: EventTarget::new_inherited(),
                buffers: DomRefCell::new(Vec::new()),
            }),
            window,
            cx,
        )
    }

    fn queue_event(&self, name: &'static str) {
        let this = Trusted::new(self);
        self.global()
            .task_manager()
            .media_element_task_source()
            .queue(task!(source_buffer_list_event: move |cx| {
                this.root().upcast::<EventTarget>().fire_event(cx, Atom::from(name));
            }));
    }

    pub(crate) fn add(&self, buffer: &SourceBuffer) {
        self.buffers.borrow_mut().push(Dom::from_ref(buffer));
        self.queue_event("addsourcebuffer");
    }

    pub(crate) fn remove(&self, buffer: &SourceBuffer) {
        let before = self.buffers.borrow().len();
        self.buffers.borrow_mut().retain(|b| &**b != buffer);
        if self.buffers.borrow().len() != before {
            self.queue_event("removesourcebuffer");
        }
    }

    pub(crate) fn clear(&self) {
        let buffers: Vec<DomRoot<SourceBuffer>> = self
            .buffers
            .borrow()
            .iter()
            .map(|b| DomRoot::from_ref(&**b))
            .collect();
        for buffer in buffers {
            buffer.abort_for_removal();
            self.remove(&buffer);
        }
    }

    pub(crate) fn contains(&self, buffer: &SourceBuffer) -> bool {
        self.buffers.borrow().iter().any(|b| &**b == buffer)
    }

    pub(crate) fn any_updating(&self) -> bool {
        self.buffers.borrow().iter().any(|b| b.updating())
    }

    pub(crate) fn highest_end_time(&self) -> f64 {
        self.buffers
            .borrow()
            .iter()
            .map(|b| b.highest_end_time())
            .fold(0.0, f64::max)
    }
}

impl SourceBufferListMethods<crate::DomTypeHolder> for SourceBufferList {
    /// <https://w3c.github.io/media-source/#dom-sourcebufferlist-length>
    fn Length(&self) -> u32 {
        self.buffers.borrow().len() as u32
    }

    // https://w3c.github.io/media-source/#dom-sourcebufferlist-onaddsourcebuffer
    event_handler!(addsourcebuffer, GetOnaddsourcebuffer, SetOnaddsourcebuffer);
    // https://w3c.github.io/media-source/#dom-sourcebufferlist-onremovesourcebuffer
    event_handler!(
        removesourcebuffer,
        GetOnremovesourcebuffer,
        SetOnremovesourcebuffer
    );

    /// <https://w3c.github.io/media-source/#dfn-sourcebufferlist-getter>
    fn IndexedGetter(&self, index: u32) -> Option<DomRoot<SourceBuffer>> {
        self.buffers
            .borrow()
            .get(index as usize)
            .map(|b| DomRoot::from_ref(&**b))
    }
}
