//! Which `close(2)` of a `/dev/dsp` fd plays out its queue asleep.
//!
//! `DspDev`'s `Drop` syncs, as Linux `snd_pcm_oss_release` does, but it
//! cannot sleep: it spun one CPU through the whole tail of the stream every
//! time an OSS player closed its fd. `close` now does that wait first, asleep
//! -- but only when the fd is the last reference to the open file, as in
//! Linux. A `dup` or a `fork` keeps the stream playing past this close.

use super::*;
use alloc::string::String;
use linux_object::fs::{File, OpenFlags};
use zcore_drivers::scheme::{AudioScheme, Scheme};
use zcore_drivers::DeviceResult;

/// A ring that plays nothing and holds nothing: enough to open a client.
struct Silent;

impl Scheme for Silent {
    fn name(&self) -> &str {
        "silent"
    }
}

impl AudioScheme for Silent {
    fn set_params(&self, rate: u32, channels: u8) -> DeviceResult<(u32, u8)> {
        Ok((rate, channels))
    }
    fn params(&self) -> (u32, u8) {
        (48_000, 2)
    }
    fn write(&self, pcm: &[u8]) -> DeviceResult<usize> {
        Ok(pcm.len())
    }
    fn free_bytes(&self) -> usize {
        4096
    }
    fn buffer_bytes(&self) -> usize {
        4096
    }
    fn queued_bytes(&self) -> usize {
        0
    }
    fn is_playing(&self) -> bool {
        false
    }
    fn reset(&self) -> DeviceResult {
        Ok(())
    }
}

fn a_dsp_client() -> Arc<dyn FileLike> {
    let node = DspDev::new(Arc::new(Silent), 0);
    let client = node.open_client().unwrap();
    File::new(client, OpenFlags::WRONLY, String::from("/dev/dsp"))
}

#[test]
fn the_last_close_of_a_client_is_the_one_that_syncs() {
    let file = a_dsp_client();
    // The fd table's handle and the one `close` looked up.
    let looked_up = file.clone();
    assert!(dsp_released_by_close(&looked_up).is_some());
}

#[test]
fn a_close_that_leaves_a_dup_open_does_not_wait() {
    let file = a_dsp_client();
    let dup = file.clone();
    let looked_up = file.clone();
    assert!(dsp_released_by_close(&looked_up).is_none());
    drop(dup);
}

#[test]
fn the_shared_dsp_node_itself_never_syncs_on_close() {
    let node: Arc<dyn INode> = Arc::new(DspDev::new(Arc::new(Silent), 0));
    let file: Arc<dyn FileLike> = File::new(node, OpenFlags::WRONLY, String::from("/dev/dsp"));
    let looked_up = file.clone();
    assert!(dsp_released_by_close(&looked_up).is_none());
}
