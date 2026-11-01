//! Los tests de las esperas dormidas de `/dev/dsp`: el `write(2)`
//! bloqueante y `SNDCTL_DSP_SYNC`, tal como los sirve el despachador
//! asíncrono de syscalls.
//!
//! Las dos esperaban girando con un backoff de 250 us, y un escritor OSS
//! bloqueante (`cat musica.raw > /dev/dsp`) va un buffer por delante del
//! dispositivo: una CPU al 100% todo el rato que sonaba.

use super::super::snd::sleeping_tests::PlayingRing;
use super::*;
use core::sync::atomic::Ordering;

/// 48 kHz S16LE estéreo, el formato por defecto de `/dev/dsp`.
const BYTES_PER_MS: usize = 48 * HW_FRAME;

/// Lo que hace `sys_write` con un `/dev/dsp` bloqueante: ofrecer el resto
/// hasta que entra todo.
fn write_all(dsp: &DspDev, buf: &[u8]) -> usize {
    let mut written = 0;
    while written < buf.len() {
        let n = async_std::task::block_on(dsp.write_pcm_sleeping(&buf[written..])).unwrap();
        assert!(
            n > 0,
            "a sleeping write returned 0 with {} B left",
            buf.len() - written
        );
        written += n;
    }
    written
}

#[test]
fn a_blocking_write_three_rings_long_goes_in_whole_asleep() {
    // 10 ms de anillo, 30 ms de escritura.
    let ring = Arc::new(PlayingRing::new(480));
    let dsp = DspDev::new(ring.clone(), 0);
    let pcm = alloc::vec![0u8; 30 * BYTES_PER_MS];
    assert_eq!(write_all(&dsp, &pcm), pcm.len());
    assert_eq!(dsp.rt.lock().bytes, pcm.len() as u64);
    let looks = ring.looks.load(Ordering::Relaxed);
    assert!(looks < 400, "the writer looked at the ring {} times", looks);
}

#[test]
fn a_held_stream_still_answers_eagain_instead_of_sleeping() {
    let ring = Arc::new(PlayingRing::new(480));
    let dsp = DspDev::new(ring, 0);
    let pcm = alloc::vec![0u8; 30 * BYTES_PER_MS];
    // Lleno el buffer y, con el disparo quitado, el sitio no va a llegar.
    let first = async_std::task::block_on(dsp.write_pcm_sleeping(&pcm)).unwrap();
    assert!(first > 0);
    dsp.rt.lock().trigger = false;
    let mut rest = &pcm[first..];
    // Puede quedar sitio de un fragmento; lo que no quepa es EAGAIN.
    loop {
        match async_std::task::block_on(dsp.write_pcm_sleeping(rest)) {
            Ok(n) => rest = &rest[n..],
            Err(e) => {
                assert_eq!(e, crate::error::LxError::EAGAIN);
                break;
            }
        }
    }
}

#[test]
fn a_sync_plays_the_queue_out_asleep() {
    let ring = Arc::new(PlayingRing::new(960));
    let dsp = DspDev::new(ring.clone(), 0);
    let pcm = alloc::vec![0u8; 20 * BYTES_PER_MS];
    write_all(&dsp, &pcm);
    assert!(ring.queued_now() > 0);
    let looks_before = ring.looks.load(Ordering::Relaxed);
    async_std::task::block_on(dsp.sync_sleeping()).unwrap();
    assert_eq!(ring.queued_now(), 0);
    assert!(
        dsp.rt.lock().prepare,
        "SYNC leaves the stream to be prepared again"
    );
    let looks = ring.looks.load(Ordering::Relaxed) - looks_before;
    assert!(looks < 200, "the sync looked at the ring {} times", looks);
}

#[test]
fn only_sync_is_the_blocking_oss_ioctl() {
    assert!(DspDev::is_sync_ioctl(SNDCTL_DSP_SYNC));
    assert!(!DspDev::is_sync_ioctl(SNDCTL_DSP_RESET));
    assert!(!DspDev::is_sync_ioctl(SNDCTL_DSP_POST));
}

#[test]
fn a_client_synced_asleep_before_close_drops_without_spinning() {
    // What `close(2)` now does: `sync_sleeping`, then the drop. The drop's
    // own sync finds the queue empty and does not wait.
    let ring = Arc::new(PlayingRing::new(960));
    let mut dsp = DspDev::new(ring.clone(), 0);
    dsp.client = true;
    assert!(dsp.syncs_on_close());
    let pcm = alloc::vec![0u8; 20 * BYTES_PER_MS];
    write_all(&dsp, &pcm);
    async_std::task::block_on(dsp.sync_sleeping()).unwrap();
    let before = ring.looks.load(Ordering::Relaxed);
    let start = kernel_hal::timer::timer_now();
    drop(dsp);
    assert!(kernel_hal::timer::timer_now() - start < core::time::Duration::from_millis(5));
    let looks = ring.looks.load(Ordering::Relaxed) - before;
    assert!(looks < 5, "the drop looked at the ring {} times", looks);
}

#[test]
fn only_a_client_syncs_on_close() {
    let ring = Arc::new(PlayingRing::new(480));
    assert!(!DspDev::new(ring, 0).syncs_on_close());
}
