/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Media Source Extensions for the GStreamer player (Eclipse OS addition).
//!
//! Each source buffer owns a small *append pipeline*: the bytes the page
//! appends are pushed straight into a demuxer (`qtdemux` for MP4,
//! `matroskademux` for WebM) and the demuxed samples land in a per-track
//! sample store. Pushing into a pad is synchronous, so when the push returns
//! every complete sample in the appended bytes is in the store and the
//! buffered ranges are exact.
//!
//! The playback pipeline (playbin3 via GstPlay) reads `servomse://`, which is
//! [`ServoMseSrc`]: a bin with one `appsrc` per track, in TIME format and
//! seekable. A feeder thread per track pushes stored samples in decode order;
//! a seek moves the track cursor to the keyframe before the target, which is
//! how WebKit and Gecko do it too.

use std::collections::BTreeMap;
use std::ops::{Bound, Range};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread;

use glib::prelude::*;
use gstreamer::prelude::*;
use servo_media_player::{AppendResult, PlayerError};

const NS: f64 = 1_000_000_000.0;
/// How far ahead of the playhead the feeder may queue, per track.
const MAX_QUEUED_TIME_NS: u64 = 3_000_000_000;
const MAX_QUEUED_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum TrackKind {
    Video,
    Audio,
}

impl TrackKind {
    fn from_caps(caps: &gstreamer::CapsRef) -> Option<Self> {
        let name = caps.structure(0)?.name();
        if name.starts_with("video/") {
            Some(TrackKind::Video)
        } else if name.starts_with("audio/") {
            Some(TrackKind::Audio)
        } else {
            None
        }
    }
}

/// Decode-order key: (dts, pts) in nanoseconds.
type Key = (u64, u64);

#[derive(Clone)]
struct Sample {
    pts: u64,
    duration: u64,
    keyframe: bool,
    caps: gstreamer::Caps,
    buffer: gstreamer::Buffer,
}

#[derive(Default)]
struct Track {
    samples: BTreeMap<Key, Sample>,
    /// Next sample to push is the first one inside this bound.
    cursor: Option<Bound<Key>>,
    appsrc: Option<gstreamer_app::AppSrc>,
    last_caps: Option<gstreamer::Caps>,
    eos_sent: bool,
    removed: bool,
    /// Bumped each time the track gets a new appsrc; an older feeder exits.
    generation: u64,
    /// Bumped by every seek.
    seeks: u64,
    /// The appsrc queue has room (need-data / enough-data). We do not use
    /// appsrc's blocking push: a push blocked on a full queue is not woken
    /// when a seek flushes that queue, which deadlocks the track.
    full: bool,
}

impl Track {
    fn buffered(&self) -> Vec<Range<u64>> {
        let mut spans: Vec<Range<u64>> = self
            .samples
            .values()
            .map(|s| s.pts..s.pts + s.duration.max(1))
            .collect();
        spans.sort_by_key(|r| r.start);
        let mut out: Vec<Range<u64>> = Vec::new();
        for span in spans {
            // Up to 2 frames (or 100 ms) of jitter between samples is not a gap.
            let slack = (2 * (span.end - span.start)).max(100_000_000);
            match out.last_mut() {
                Some(last) if span.start <= last.end + slack => last.end = last.end.max(span.end),
                _ => out.push(span),
            }
        }
        out
    }

    /// Coded frame removal for `[start, end)` ns, plus the frames that
    /// depend on removed ones (until the next keyframe in decode order).
    fn remove(&mut self, start: u64, end: u64) {
        let doomed: Vec<Key> = self
            .samples
            .iter()
            .filter(|(_, s)| s.pts >= start && s.pts < end)
            .map(|(k, _)| *k)
            .collect();
        let Some(last) = doomed.iter().max().copied() else {
            return;
        };
        for key in &doomed {
            self.samples.remove(key);
        }
        let dependants: Vec<Key> = self
            .samples
            .range((Bound::Excluded(last), Bound::Unbounded))
            .take_while(|(_, s)| !s.keyframe)
            .map(|(k, _)| *k)
            .collect();
        for key in dependants {
            self.samples.remove(&key);
        }
    }

    /// Moves the cursor to the keyframe at or before `target` ns.
    fn seek(&mut self, target: u64) {
        let mut best: Option<Key> = None;
        for (key, sample) in &self.samples {
            if sample.keyframe && sample.pts <= target {
                if best.is_none_or(|b| sample.pts >= self.samples[&b].pts) {
                    best = Some(*key);
                }
            }
        }
        let best = best.or_else(|| {
            self.samples
                .iter()
                .find(|(_, s)| s.keyframe && s.pts >= target)
                .map(|(k, _)| *k)
        });
        self.cursor = Some(match best {
            Some(key) => Bound::Included(key),
            // Nothing buffered there yet: wait for the page to append it.
            None => Bound::Included((target, 0)),
        });
        self.eos_sent = false;
        self.seeks += 1;
        // The seek flushes the appsrc queue.
        self.full = false;
    }
}

#[derive(Default)]
struct Collected {
    samples: Vec<(TrackKind, Sample)>,
}

struct AppendParser {
    pipeline: gstreamer::Pipeline,
    srcpad: gstreamer::Pad,
    collected: Arc<Mutex<Collected>>,
}

impl AppendParser {
    fn new(id: u32, mime: &str) -> Result<Self, PlayerError> {
        let (demux, caps) = demuxer_for(mime)
            .ok_or_else(|| PlayerError::Backend(format!("MSE: unsupported type {mime}")))?;
        let pipeline = gstreamer::Pipeline::with_name(&format!("mse-append-{id}"));
        let demux = gstreamer::ElementFactory::make(demux)
            .build()
            .map_err(|e| PlayerError::Backend(format!("MSE: {demux}: {e}")))?;
        pipeline
            .add(&demux)
            .map_err(|e| PlayerError::Backend(e.to_string()))?;

        let collected = Arc::new(Mutex::new(Collected::default()));
        let weak_pipeline = pipeline.downgrade();
        let collected_ = collected.clone();
        demux.connect_pad_added(move |_, pad| {
            let Some(pipeline) = weak_pipeline.upgrade() else {
                return;
            };
            let Ok(sink) = gstreamer::ElementFactory::make("appsink")
                .property("sync", false)
                .property("async", false)
                .property("enable-last-sample", false)
                .build()
            else {
                return;
            };
            let sink = sink.dynamic_cast::<gstreamer_app::AppSink>().unwrap();
            let collected = collected_.clone();
            sink.set_callbacks(
                gstreamer_app::AppSinkCallbacks::builder()
                    .new_sample(move |sink| {
                        let sample = sink.pull_sample().map_err(|_| gstreamer::FlowError::Eos)?;
                        let (Some(buffer), Some(caps)) =
                            (sample.buffer_owned(), sample.caps_owned())
                        else {
                            return Ok(gstreamer::FlowSuccess::Ok);
                        };
                        let Some(kind) = TrackKind::from_caps(&caps) else {
                            return Ok(gstreamer::FlowSuccess::Ok);
                        };
                        let Some(pts) = buffer.pts().or(buffer.dts()) else {
                            return Ok(gstreamer::FlowSuccess::Ok);
                        };
                        let sample = Sample {
                            pts: pts.nseconds(),
                            duration: buffer.duration().map(|d| d.nseconds()).unwrap_or(0),
                            keyframe: !buffer.flags().contains(gstreamer::BufferFlags::DELTA_UNIT),
                            caps,
                            buffer,
                        };
                        collected.lock().unwrap().samples.push((kind, sample));
                        Ok(gstreamer::FlowSuccess::Ok)
                    })
                    .build(),
            );
            if pipeline.add(&sink).is_err() {
                return;
            }
            let _ = sink.sync_state_with_parent();
            if let Some(sinkpad) = sink.static_pad("sink") {
                let _ = pad.link(&sinkpad);
            }
        });

        let srcpad = gstreamer::Pad::builder(gstreamer::PadDirection::Src)
            .name(format!("mse-push-{id}"))
            .build();
        let sinkpad = demux
            .static_pad("sink")
            .ok_or_else(|| PlayerError::Backend("MSE: demuxer without sink pad".into()))?;
        srcpad
            .link(&sinkpad)
            .map_err(|e| PlayerError::Backend(format!("MSE: link: {e:?}")))?;
        srcpad
            .set_active(true)
            .map_err(|e| PlayerError::Backend(e.to_string()))?;
        pipeline
            .set_state(gstreamer::State::Playing)
            .map_err(|e| PlayerError::Backend(format!("MSE: append pipeline: {e:?}")))?;

        srcpad.push_event(gstreamer::event::StreamStart::new(&format!("mse-{id}")));
        srcpad.push_event(gstreamer::event::Caps::new(&caps));
        let segment = gstreamer::FormattedSegment::<gstreamer::format::Bytes>::new();
        srcpad.push_event(gstreamer::event::Segment::new(segment.upcast_ref()));

        Ok(Self {
            pipeline,
            srcpad,
            collected,
        })
    }

    /// Demuxes `data`; returns the complete samples it contained.
    fn push(&self, data: Vec<u8>) -> Result<Vec<(TrackKind, Sample)>, ()> {
        let flow = self.srcpad.push(gstreamer::Buffer::from_slice(data));
        let mut error = flow.is_err() && flow != Err(gstreamer::FlowError::NotLinked);
        if let Some(bus) = self.pipeline.bus() {
            while let Some(msg) = bus.pop() {
                if let gstreamer::MessageView::Error(err) = msg.view() {
                    log::warn!("MSE demuxer error: {}", err.error());
                    error = true;
                }
            }
        }
        let samples = std::mem::take(&mut self.collected.lock().unwrap().samples);
        if error { Err(()) } else { Ok(samples) }
    }
}

impl Drop for AppendParser {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gstreamer::State::Null);
    }
}

/// Picks the demuxer and the container caps for an MSE type string.
pub fn demuxer_for(mime: &str) -> Option<(&'static str, gstreamer::Caps)> {
    let essence = mime.split(';').next()?.trim().to_ascii_lowercase();
    match essence.as_str() {
        "video/mp4" | "audio/mp4" => Some((
            "qtdemux",
            gstreamer::Caps::builder("video/quicktime")
                .field("variant", "iso")
                .build(),
        )),
        "video/webm" | "audio/webm" => Some((
            "matroskademux",
            gstreamer::Caps::builder(essence.as_str()).build(),
        )),
        _ => None,
    }
}

/// The bytes of `data` that form an initialization segment, if it starts
/// with one: `ftyp`+`moov` for MP4, EBML header up to the first Cluster for
/// WebM. Kept so a reset parser can be primed again after `abort()`.
pub fn init_segment(data: &[u8]) -> Option<&[u8]> {
    if data.len() >= 8 && (&data[4..8] == b"ftyp" || &data[4..8] == b"moov") {
        let mut offset = 0usize;
        let mut end = None;
        while offset + 8 <= data.len() {
            let size = u32::from_be_bytes(data[offset..offset + 4].try_into().ok()?) as usize;
            let kind = &data[offset + 4..offset + 8];
            if size < 8 || offset + size > data.len() {
                break;
            }
            match kind {
                b"moof" | b"sidx" | b"styp" | b"mdat" | b"emsg" => break,
                b"moov" => end = Some(offset + size),
                _ => {}
            }
            offset += size;
        }
        return end.map(|end| &data[..end]);
    }
    if data.starts_with(&[0x1A, 0x45, 0xDF, 0xA3]) {
        let cluster = data.windows(4).position(|w| w == [0x1F, 0x43, 0xB6, 0x75]);
        return Some(&data[..cluster.unwrap_or(data.len())]);
    }
    None
}

struct SourceBuffer {
    mime: String,
    init: Option<Vec<u8>>,
    parser: Option<Arc<Mutex<AppendParser>>>,
    tracks: BTreeMap<TrackKind, Track>,
}

/// A track whose appsrc still has to be added to the source bin.
struct PendingTrack {
    src: ServoMseSrc,
    appsrc: gstreamer_app::AppSrc,
    id: u32,
    kind: TrackKind,
    generation: u64,
}

#[derive(Default)]
struct Inner {
    buffers: BTreeMap<u32, SourceBuffer>,
    src: Option<ServoMseSrc>,
    duration: Option<f64>,
    ended: bool,
    pads_done: bool,
    shutdown: bool,
}

/// MSE state shared by the player, its source element and the feeders.
#[derive(Clone, Default)]
pub struct MediaSourceState {
    inner: Arc<(Mutex<Inner>, Condvar)>,
}

fn ns_ranges_to_secs(ranges: &[Range<u64>]) -> Vec<Range<f64>> {
    ranges
        .iter()
        .map(|r| r.start as f64 / NS..r.end as f64 / NS)
        .collect()
}

fn intersect(a: &[Range<u64>], b: &[Range<u64>]) -> Vec<Range<u64>> {
    let mut out = Vec::new();
    for x in a {
        for y in b {
            let start = x.start.max(y.start);
            let end = x.end.min(y.end);
            if start < end {
                out.push(start..end);
            }
        }
    }
    out
}

fn buffer_ranges(buffer: &SourceBuffer) -> Vec<Range<u64>> {
    let mut tracks = buffer.tracks.values().filter(|t| !t.removed);
    let Some(first) = tracks.next() else {
        return Vec::new();
    };
    tracks.fold(first.buffered(), |acc, t| intersect(&acc, &t.buffered()))
}

impl MediaSourceState {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.0.lock().unwrap()
    }

    fn notify(&self) {
        self.inner.1.notify_all();
    }

    pub fn add_source_buffer(&self, id: u32, mime: &str) -> Result<(), PlayerError> {
        demuxer_for(mime)
            .ok_or_else(|| PlayerError::Backend(format!("MSE: unsupported type {mime}")))?;
        let mut inner = self.lock();
        inner.buffers.insert(
            id,
            SourceBuffer {
                mime: mime.to_owned(),
                init: None,
                parser: None,
                tracks: BTreeMap::new(),
            },
        );
        Ok(())
    }

    pub fn remove_source_buffer(&self, id: u32) {
        let mut inner = self.lock();
        if let Some(mut buffer) = inner.buffers.remove(&id) {
            for track in buffer.tracks.values_mut() {
                track.removed = true;
            }
        }
        drop(inner);
        self.notify();
    }

    pub fn reset_parser(&self, id: u32) {
        if let Some(buffer) = self.lock().buffers.get_mut(&id) {
            buffer.parser = None;
        }
    }

    pub fn set_duration(&self, duration: f64) {
        let mut inner = self.lock();
        inner.duration = Some(duration);
        let ns = (duration.is_finite() && duration >= 0.0)
            .then(|| gstreamer::ClockTime::from_nseconds((duration * NS) as u64));
        let appsrcs: Vec<_> = inner
            .buffers
            .values()
            .flat_map(|b| b.tracks.values())
            .filter_map(|t| t.appsrc.clone())
            .collect();
        drop(inner);
        for appsrc in appsrcs {
            appsrc.set_duration(ns);
        }
    }

    pub fn set_ended(&self, ended: bool) {
        self.lock().ended = ended;
        self.notify();
    }

    pub fn buffered(&self) -> Vec<Range<f64>> {
        let inner = self.lock();
        let mut buffers = inner.buffers.values();
        let Some(first) = buffers.next() else {
            return Vec::new();
        };
        let ranges = buffers.fold(buffer_ranges(first), |acc, b| {
            intersect(&acc, &buffer_ranges(b))
        });
        ns_ranges_to_secs(&ranges)
    }

    pub fn remove_range(&self, id: u32, range: Range<f64>) -> Vec<Range<f64>> {
        let mut inner = self.lock();
        let Some(buffer) = inner.buffers.get_mut(&id) else {
            return Vec::new();
        };
        let start = (range.start.max(0.0) * NS) as u64;
        let end = if range.end.is_finite() {
            (range.end * NS) as u64
        } else {
            u64::MAX
        };
        for track in buffer.tracks.values_mut() {
            track.remove(start, end);
        }
        ns_ranges_to_secs(&buffer_ranges(buffer))
    }

    pub fn append(
        &self,
        id: u32,
        data: Vec<u8>,
        timestamp_offset: f64,
        window: Range<f64>,
    ) -> Result<AppendResult, PlayerError> {
        let mut result = AppendResult::default();
        let parser = {
            let mut inner = self.lock();
            let buffer = inner
                .buffers
                .get_mut(&id)
                .ok_or_else(|| PlayerError::Backend("MSE: no such source buffer".into()))?;
            if let Some(init) = init_segment(&data) {
                buffer.init = Some(init.to_vec());
                result.init_segment = true;
            }
            match &buffer.parser {
                Some(parser) => parser.clone(),
                None => {
                    let parser = Arc::new(Mutex::new(AppendParser::new(id, &buffer.mime)?));
                    // After abort(): replay the last init segment so the new
                    // demuxer knows the tracks, unless this append brings one.
                    if !result.init_segment {
                        if let Some(init) = buffer.init.clone() {
                            let _ = parser.lock().unwrap().push(init);
                        }
                    }
                    buffer.parser = Some(parser.clone());
                    parser
                }
            }
        };

        let samples = parser.lock().unwrap().push(data);
        let mut inner = self.lock();
        let Some(buffer) = inner.buffers.get_mut(&id) else {
            return Ok(result);
        };
        let samples = match samples {
            Ok(samples) => samples,
            Err(()) => {
                buffer.parser = None;
                result.error = true;
                return Ok(result);
            }
        };

        let offset_ns = (timestamp_offset * NS) as i64;
        let window_start = (window.start.max(0.0) * NS) as u64;
        let window_end = if window.end.is_finite() {
            (window.end * NS) as u64
        } else {
            u64::MAX
        };

        let mut by_track: BTreeMap<TrackKind, Vec<(Key, Sample)>> = BTreeMap::new();
        for (kind, mut sample) in samples {
            let shift = |t: u64| (t as i64).saturating_add(offset_ns).max(0) as u64;
            let pts = shift(sample.pts);
            let dts = sample
                .buffer
                .dts()
                .map(|d| shift(d.nseconds()))
                .unwrap_or(pts);
            if pts < window_start || pts.saturating_add(sample.duration) > window_end {
                continue;
            }
            if offset_ns != 0 {
                let buffer = sample.buffer.make_mut();
                buffer.set_pts(gstreamer::ClockTime::from_nseconds(pts));
                buffer.set_dts(gstreamer::ClockTime::from_nseconds(dts));
            }
            sample.pts = pts;
            by_track.entry(kind).or_default().push(((dts, pts), sample));
        }

        let mut new_tracks = Vec::new();
        for (kind, samples) in by_track {
            let track = buffer.tracks.entry(kind).or_insert_with(|| {
                new_tracks.push(kind);
                Track::default()
            });
            // Frames without a duration span until the next one.
            let mut samples = samples;
            samples.sort_by_key(|(_, s)| s.pts);
            for i in 0..samples.len() {
                if samples[i].1.duration == 0 {
                    let next = samples.get(i + 1).map(|(_, s)| s.pts);
                    samples[i].1.duration = next
                        .map(|n| n.saturating_sub(samples[i].1.pts))
                        .unwrap_or(20_000_000);
                }
            }
            let start = samples.iter().map(|(_, s)| s.pts).min().unwrap_or(0);
            let end = samples
                .iter()
                .map(|(_, s)| s.pts + s.duration)
                .max()
                .unwrap_or(0);
            track.remove(start, end);
            for (key, sample) in samples {
                track.samples.insert(key, sample);
            }
            track.eos_sent = false;
        }
        result.buffered = ns_ranges_to_secs(&buffer_ranges(buffer));
        inner.ended = false;

        let adds = if new_tracks.is_empty() {
            Vec::new()
        } else {
            self.expose_tracks(&mut inner, id, &new_tracks)
        };
        drop(inner);
        self.finish_exposing(adds);
        self.notify();
        Ok(result)
    }

    /// Called from `source-setup`: the playback source exists now.
    pub fn attach(&self, src: ServoMseSrc) {
        let mut inner = self.lock();
        inner.src = Some(src);
        inner.pads_done = false;
        // A new pipeline (after stop()): every track needs a new appsrc and
        // restarts from the beginning unless a seek moves it.
        for track in inner
            .buffers
            .values_mut()
            .flat_map(|b| b.tracks.values_mut())
        {
            track.appsrc = None;
            track.cursor = None;
            track.eos_sent = false;
        }
        let pending: Vec<(u32, TrackKind)> = inner
            .buffers
            .iter()
            .flat_map(|(id, b)| b.tracks.keys().map(move |k| (*id, *k)))
            .collect();
        let mut adds = Vec::new();
        for (id, kind) in pending {
            adds.extend(self.expose_tracks(&mut inner, id, &[kind]));
        }
        drop(inner);
        self.finish_exposing(adds);
    }

    /// Adds the new appsrcs to the source bin and starts their feeders, then
    /// signals no-more-pads once every source buffer has a track. Runs
    /// without the lock: adding a pad or starting an appsrc can call back
    /// into `seek_data` on this thread.
    fn finish_exposing(&self, adds: Vec<PendingTrack>) {
        for add in adds {
            add.src.add_track(&add.appsrc);
            let state = self.clone();
            let (id, kind, generation) = (add.id, add.kind, add.generation);
            let _ = thread::Builder::new()
                .name(format!("mse-feed-{id}-{kind:?}"))
                .spawn(move || state.feed(id, kind, generation));
        }
        let src = {
            let mut inner = self.lock();
            self.take_finished_pads(&mut inner)
        };
        if let Some(src) = src {
            src.no_more_pads();
        }
    }

    fn expose_tracks(&self, inner: &mut Inner, id: u32, kinds: &[TrackKind]) -> Vec<PendingTrack> {
        let mut adds = Vec::new();
        let Some(src) = inner.src.clone() else {
            return adds;
        };
        let duration = inner.duration;
        for kind in kinds {
            let Some(track) = inner
                .buffers
                .get_mut(&id)
                .and_then(|b| b.tracks.get_mut(kind))
            else {
                continue;
            };
            if track.appsrc.is_some() {
                continue;
            }
            let Some(caps) = track.samples.values().next().map(|s| s.caps.clone()) else {
                continue;
            };
            let Ok(appsrc) = gstreamer::ElementFactory::make("appsrc").build() else {
                continue;
            };
            let appsrc = appsrc.dynamic_cast::<gstreamer_app::AppSrc>().unwrap();
            appsrc.set_format(gstreamer::Format::Time);
            appsrc.set_stream_type(gstreamer_app::AppStreamType::Seekable);
            appsrc.set_caps(Some(&caps));
            appsrc.set_block(false);
            appsrc.set_max_bytes(MAX_QUEUED_BYTES);
            appsrc.set_property("max-time", MAX_QUEUED_TIME_NS);
            appsrc.set_property("is-live", false);
            if let Some(d) = duration.filter(|d| d.is_finite() && *d >= 0.0) {
                appsrc.set_duration(gstreamer::ClockTime::from_nseconds((d * NS) as u64));
            }
            let state = self.clone();
            let kind_ = *kind;
            let (state_need, state_enough) = (self.clone(), self.clone());
            let kind_need = *kind;
            appsrc.set_callbacks(
                gstreamer_app::AppSrcCallbacks::builder()
                    .need_data(move |_, _| state_need.set_full(id, kind_need, false))
                    .enough_data(move |_| state_enough.set_full(id, kind_need, true))
                    .seek_data(move |_, offset| {
                        let mut inner = state.lock();
                        if let Some(track) = inner
                            .buffers
                            .get_mut(&id)
                            .and_then(|b| b.tracks.get_mut(&kind_))
                        {
                            track.seek(offset);
                        }
                        drop(inner);
                        state.notify();
                        true
                    })
                    .build(),
            );
            track.last_caps = Some(caps);
            track.appsrc = Some(appsrc.clone());
            track.generation += 1;
            adds.push(PendingTrack {
                src: src.clone(),
                appsrc,
                id,
                kind: *kind,
                generation: track.generation,
            });
        }
        adds
    }

    fn set_full(&self, id: u32, kind: TrackKind, full: bool) {
        if let Some(track) = self
            .lock()
            .buffers
            .get_mut(&id)
            .and_then(|b| b.tracks.get_mut(&kind))
        {
            track.full = full;
        }
        if !full {
            self.notify();
        }
    }

    fn take_finished_pads(&self, inner: &mut Inner) -> Option<ServoMseSrc> {
        if inner.pads_done || inner.src.is_none() || inner.buffers.is_empty() {
            return None;
        }
        let ready = inner
            .buffers
            .values()
            .all(|b| b.tracks.values().any(|t| t.appsrc.is_some()));
        if ready {
            inner.pads_done = true;
            return inner.src.clone();
        }
        None
    }

    /// Feeder thread: pushes the track's samples in decode order.
    fn feed(&self, id: u32, kind: TrackKind, generation: u64) {
        let mut inner = self.lock();
        loop {
            if inner.shutdown {
                return;
            }
            let ended = inner.ended;
            let Some(track) = inner
                .buffers
                .get_mut(&id)
                .and_then(|b| b.tracks.get_mut(&kind))
                .filter(|t| !t.removed && t.generation == generation)
            else {
                return;
            };
            let Some(appsrc) = track.appsrc.clone() else {
                return;
            };
            let bound = track.cursor.unwrap_or(Bound::Unbounded);
            let next = track
                .samples
                .range((bound, Bound::Unbounded))
                .next()
                .map(|(k, s)| (*k, s.clone()));
            let seeks = track.seeks;
            match next {
                Some(_) if track.full => {
                    inner = self.inner.1.wait(inner).unwrap();
                }
                Some((key, sample)) => {
                    track.cursor = Some(Bound::Excluded(key));
                    let caps_changed = track.last_caps.as_ref() != Some(&sample.caps);
                    if caps_changed {
                        track.last_caps = Some(sample.caps.clone());
                    }
                    drop(inner);
                    if caps_changed {
                        appsrc.set_caps(Some(&sample.caps));
                    }
                    let flow = appsrc.push_buffer(sample.buffer);
                    inner = self.lock();
                    if flow == Err(gstreamer::FlowError::Flushing) {
                        // A seek is flushing the appsrc. Unless its seek-data
                        // already moved the cursor, this sample has to go
                        // again once the flush is over (it may be the
                        // keyframe the seek landed on).
                        inner = self.retry_later(inner, id, kind, seeks, Some(key));
                    }
                }
                None => {
                    if ended && !track.eos_sent {
                        track.eos_sent = true;
                        drop(inner);
                        let flow = appsrc.end_of_stream();
                        inner = self.lock();
                        if flow == Err(gstreamer::FlowError::Flushing) {
                            inner = self.retry_later(inner, id, kind, seeks, None);
                        }
                        continue;
                    }
                    inner = self.inner.1.wait(inner).unwrap();
                }
            }
        }
    }

    /// After a push hit a flush: put the sample (or the EOS) back unless a
    /// seek moved the cursor meanwhile, then wait a little for the flush to end.
    fn retry_later<'a>(
        &'a self,
        mut inner: MutexGuard<'a, Inner>,
        id: u32,
        kind: TrackKind,
        seeks: u64,
        key: Option<Key>,
    ) -> MutexGuard<'a, Inner> {
        if let Some(track) = inner
            .buffers
            .get_mut(&id)
            .and_then(|b| b.tracks.get_mut(&kind))
        {
            if track.seeks == seeks {
                match key {
                    Some(key) => track.cursor = Some(Bound::Included(key)),
                    None => track.eos_sent = false,
                }
            }
        }
        self.inner
            .1
            .wait_timeout(inner, std::time::Duration::from_millis(10))
            .unwrap()
            .0
    }

    pub fn shutdown(&self) {
        let mut inner = self.lock();
        inner.shutdown = true;
        inner.buffers.clear();
        inner.src = None;
        drop(inner);
        self.notify();
    }
}

mod imp {
    use std::sync::LazyLock;

    use gstreamer::subclass::prelude::*;
    use url::Url;

    use super::*;

    #[derive(Default)]
    pub struct ServoMseSrc {
        pub(super) next_pad: Mutex<u32>,
        last_seek: Mutex<Option<gstreamer::Seqnum>>,
    }

    impl ServoMseSrc {
        /// Sends a seek to every track's appsrc, once per seqnum. decodebin3
        /// forwards a pipeline seek through one of our pads only, but every
        /// track has to flush and move its cursor.
        pub(super) fn seek_all(&self, event: gstreamer::Event) -> bool {
            {
                let mut last = self.last_seek.lock().unwrap();
                if *last == Some(event.seqnum()) {
                    return true;
                }
                *last = Some(event.seqnum());
            }
            let mut handled = false;
            for pad in self.obj().src_pads() {
                let target = pad
                    .downcast_ref::<gstreamer::GhostPad>()
                    .and_then(|ghost| ghost.target());
                if let Some(target) = target {
                    handled |= target.send_event(event.clone());
                }
            }
            handled
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for ServoMseSrc {
        const NAME: &'static str = "ServoMseSrc";
        type Type = super::ServoMseSrc;
        type ParentType = gstreamer::Bin;
        type Interfaces = (gstreamer::URIHandler,);
    }

    impl ObjectImpl for ServoMseSrc {
        fn constructed(&self) {
            self.parent_constructed();
            self.obj()
                .set_element_flags(gstreamer::ElementFlags::SOURCE);
        }
    }

    impl GstObjectImpl for ServoMseSrc {}

    impl ElementImpl for ServoMseSrc {
        fn metadata() -> Option<&'static gstreamer::subclass::ElementMetadata> {
            static META: LazyLock<gstreamer::subclass::ElementMetadata> = LazyLock::new(|| {
                gstreamer::subclass::ElementMetadata::new(
                    "Servo MSE Source",
                    "Source/Audio/Video",
                    "Plays Media Source Extensions buffers",
                    "Servo developers",
                )
            });
            Some(&*META)
        }

        fn pad_templates() -> &'static [gstreamer::PadTemplate] {
            static TEMPLATES: LazyLock<Vec<gstreamer::PadTemplate>> = LazyLock::new(|| {
                vec![
                    gstreamer::PadTemplate::new(
                        "src_%u",
                        gstreamer::PadDirection::Src,
                        gstreamer::PadPresence::Sometimes,
                        &gstreamer::Caps::new_any(),
                    )
                    .unwrap(),
                ]
            });
            TEMPLATES.as_ref()
        }

        /// A seek sent to the element must reach every track. The default
        /// handler forwards upstream events to one random source pad only.
        fn send_event(&self, event: gstreamer::Event) -> bool {
            if let gstreamer::EventView::Seek(_) = event.view() {
                return self.seek_all(event);
            }
            self.parent_send_event(event)
        }
    }

    impl BinImpl for ServoMseSrc {}

    impl URIHandlerImpl for ServoMseSrc {
        const URI_TYPE: gstreamer::URIType = gstreamer::URIType::Src;

        fn protocols() -> &'static [&'static str] {
            &["servomse"]
        }

        fn uri(&self) -> Option<String> {
            Some("servomse://".to_string())
        }

        fn set_uri(&self, uri: &str) -> Result<(), glib::Error> {
            if Url::parse(uri).is_ok_and(|u| u.scheme() == "servomse") {
                return Ok(());
            }
            Err(glib::Error::new(
                gstreamer::URIError::BadUri,
                &format!("Invalid URI '{uri}'"),
            ))
        }
    }
}

glib::wrapper! {
    pub struct ServoMseSrc(ObjectSubclass<imp::ServoMseSrc>)
        @extends gstreamer::Bin, gstreamer::Element, gstreamer::Object, @implements gstreamer::URIHandler;
}

unsafe impl Send for ServoMseSrc {}
unsafe impl Sync for ServoMseSrc {}

impl ServoMseSrc {
    fn add_track(&self, appsrc: &gstreamer_app::AppSrc) {
        use gstreamer::subclass::prelude::ObjectSubclassIsExt;
        let n = {
            let mut next = self.imp().next_pad.lock().unwrap();
            *next += 1;
            *next - 1
        };
        if self.add(appsrc).is_err() {
            return;
        }
        let Some(target) = appsrc.static_pad("src") else {
            return;
        };
        let templ = self.pad_template("src_%u").unwrap();
        let Ok(ghost) = gstreamer::GhostPad::builder_from_template_with_target(&templ, &target)
        else {
            return;
        };
        let ghost = ghost
            .name(format!("src_{n}"))
            .event_function(|pad, parent, event| {
                if let gstreamer::EventView::Seek(_) = event.view() {
                    if let Some(src) = parent.and_then(|p| p.downcast_ref::<ServoMseSrc>()) {
                        return src.imp().seek_all(event);
                    }
                }
                gstreamer::Pad::event_default(pad, parent, event)
            })
            .build();
        let _ = ghost.set_active(true);
        let _ = self.add_pad(&ghost);
        let _ = appsrc.sync_state_with_parent();
    }
}

pub fn register_servo_mse_src() -> Result<(), glib::BoolError> {
    gstreamer::Element::register(
        None,
        "servomsesrc",
        gstreamer::Rank::NONE,
        ServoMseSrc::static_type(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_mp4_and_webm_init_segments() {
        let mut mp4 = Vec::new();
        mp4.extend_from_slice(&16u32.to_be_bytes());
        mp4.extend_from_slice(b"ftypiso5");
        mp4.extend_from_slice(&[0; 4]);
        mp4.extend_from_slice(&8u32.to_be_bytes());
        mp4.extend_from_slice(b"moov");
        let init_len = mp4.len();
        mp4.extend_from_slice(&8u32.to_be_bytes());
        mp4.extend_from_slice(b"moof");
        assert_eq!(init_segment(&mp4).map(<[u8]>::len), Some(init_len));
        assert_eq!(init_segment(&mp4[init_len..]), None);

        let webm = [0x1A, 0x45, 0xDF, 0xA3, 1, 2, 0x1F, 0x43, 0xB6, 0x75, 9];
        assert_eq!(init_segment(&webm), Some(&webm[..6]));
        assert_eq!(init_segment(&webm[6..]), None);
    }

    #[test]
    fn buffered_merges_adjacent_samples_and_keeps_gaps() {
        gstreamer::init().unwrap();
        let caps = gstreamer::Caps::builder("audio/x-opus").build();
        let mut track = Track::default();
        for (i, pts) in [0u64, 20, 40, 1000, 1020].iter().enumerate() {
            let pts = pts * 1_000_000;
            track.samples.insert(
                (pts, i as u64),
                Sample {
                    pts,
                    duration: 20_000_000,
                    keyframe: true,
                    caps: caps.clone(),
                    buffer: gstreamer::Buffer::new(),
                },
            );
        }
        assert_eq!(
            track.buffered(),
            vec![0..60_000_000, 1_000_000_000..1_040_000_000]
        );
        track.remove(1_000_000_000, 2_000_000_000);
        assert_eq!(track.buffered(), vec![0..60_000_000]);
        track.seek(30_000_000);
        assert_eq!(track.cursor, Some(Bound::Included((20_000_000, 1))));
    }

    #[test]
    fn picks_demuxers_by_mime() {
        gstreamer::init().unwrap();
        assert_eq!(
            demuxer_for("video/mp4; codecs=\"avc1.4d401f\"").unwrap().0,
            "qtdemux"
        );
        assert_eq!(
            demuxer_for("audio/webm; codecs=opus").unwrap().0,
            "matroskademux"
        );
        assert!(demuxer_for("video/x-flv").is_none());
    }

    /// End-to-end: playbin3 plays `servomse://` fed from appended chunks.
    /// Needs media files: `MSE_TEST_DIR=... cargo test -p servo-media-gstreamer -- --ignored`.
    #[test]
    #[ignore]
    fn playbin_plays_appended_segments() {
        let Ok(dir) = std::env::var("MSE_TEST_DIR") else {
            return;
        };
        gstreamer::init().unwrap();
        register_servo_mse_src().unwrap();
        for (video, audio, vmime, amime) in [
            (
                "v.mp4",
                "a.mp4",
                "video/mp4; codecs=\"avc1.64000d\"",
                "audio/mp4; codecs=\"mp4a.40.2\"",
            ),
            (
                "v.webm",
                "a.webm",
                "video/webm; codecs=\"vp9\"",
                "audio/webm; codecs=\"opus\"",
            ),
        ] {
            let state = MediaSourceState::default();
            let playbin = gstreamer::ElementFactory::make("playbin3")
                .property("uri", "servomse://")
                .build()
                .unwrap();
            for sink in ["video-sink", "audio-sink"] {
                let fake = gstreamer::ElementFactory::make("fakesink")
                    .property("sync", true)
                    .build()
                    .unwrap();
                playbin.set_property(sink, &fake);
            }
            let state_ = state.clone();
            playbin.connect("source-setup", false, move |args| {
                let src = args[1].get::<gstreamer::Element>().unwrap();
                state_.attach(src.dynamic_cast::<ServoMseSrc>().unwrap());
                None
            });
            state.add_source_buffer(0, vmime).unwrap();
            state.add_source_buffer(1, amime).unwrap();
            state.set_duration(12.0);
            playbin.set_state(gstreamer::State::Paused).unwrap();
            for (id, file) in [(0, video), (1, audio)] {
                let bytes = std::fs::read(format!("{dir}/{file}")).unwrap();
                let mut last = AppendResult::default();
                // Odd chunk sizes: segments split anywhere, like a network.
                for chunk in bytes.chunks(37_000) {
                    last = state
                        .append(id, chunk.to_vec(), 0.0, 0.0..f64::INFINITY)
                        .unwrap();
                    assert!(!last.error, "{file}");
                }
                let end = last.buffered.last().map(|r| r.end).unwrap_or(0.0);
                assert!(end > 11.0, "{file} buffered {:?}", last.buffered);
            }
            state.set_ended(true);
            let bus = playbin.bus().unwrap();
            playbin.set_state(gstreamer::State::Playing).unwrap();
            let position = |playbin: &gstreamer::Element| {
                playbin
                    .query_position::<gstreamer::ClockTime>()
                    .map(|p| p.seconds_f64())
                    .unwrap_or(0.0)
            };
            let wait = |playbin: &gstreamer::Element, until: f64| {
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
                while std::time::Instant::now() < deadline {
                    if let Some(msg) = bus.timed_pop(gstreamer::ClockTime::from_mseconds(100)) {
                        if let gstreamer::MessageView::Error(e) = msg.view() {
                            panic!("{video}: {} {:?}", e.error(), e.debug());
                        }
                    }
                    if position(playbin) >= until {
                        return;
                    }
                }
                panic!("{video}: stuck at {}", position(playbin));
            };
            wait(&playbin, 1.0);
            playbin
                .seek_simple(
                    gstreamer::SeekFlags::FLUSH | gstreamer::SeekFlags::KEY_UNIT,
                    gstreamer::ClockTime::from_seconds(8),
                )
                .unwrap();
            wait(&playbin, 8.5);
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            let mut eos = false;
            while !eos && std::time::Instant::now() < deadline {
                if let Some(msg) = bus.timed_pop(gstreamer::ClockTime::from_mseconds(100)) {
                    eos = matches!(msg.view(), gstreamer::MessageView::Eos(_));
                }
            }
            assert!(eos, "{video}: no EOS, at {}", position(&playbin));
            playbin.set_state(gstreamer::State::Null).unwrap();
            state.shutdown();
        }
    }
}
