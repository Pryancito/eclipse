//! Los tests de las esperas dormidas del PCM: `WRITEI_FRAMES` bloqueante y
//! `DRAIN`, tal como los sirve el despachador asíncrono de syscalls.
//!
//! Las dos esperaban girando: un cliente con el PCM en modo bloqueante
//! (`aplay`, `mpg123 -o alsa`, SDL) tenía una CPU al 100% todo el rato que
//! sonaba, releyendo la posición del DMA. Lo que se comprueba aquí es que la
//! versión dormida escribe y vacía lo mismo, y que entre medias mira el
//! anillo unas pocas veces, no miles.

use super::*;
use core::sync::atomic::AtomicUsize;
use zcore_drivers::DeviceResult;

/// Bytes por milisegundo a 48 kHz, S16LE estéreo.
const BYTES_PER_MS: u64 = 48 * BYTES_PER_FRAME;

/// Un anillo que suena de verdad: lo encolado baja con el reloj, a 48 kHz.
struct PlayingRing {
    cap: usize,
    /// Encolado en el instante `.1`.
    queued: Mutex<(usize, Duration)>,
    /// Cuántas veces se ha mirado cuánto queda: lo que una espera que gira
    /// dispara y una que duerme no.
    looks: AtomicUsize,
}

impl PlayingRing {
    fn new(frames: usize) -> Self {
        Self {
            cap: frames * BYTES_PER_FRAME as usize,
            queued: Mutex::new((0, kernel_hal::timer::timer_now())),
            looks: AtomicUsize::new(0),
        }
    }

    fn queued_now(&self) -> usize {
        let mut q = self.queued.lock();
        let now = kernel_hal::timer::timer_now();
        let ms = now.saturating_sub(q.1).as_millis() as u64;
        if ms > 0 {
            let played = (ms * BYTES_PER_MS) as usize;
            *q = (q.0.saturating_sub(played), q.1 + Duration::from_millis(ms));
        }
        q.0
    }
}

impl zcore_drivers::scheme::Scheme for PlayingRing {
    fn name(&self) -> &str {
        "playing-ring"
    }
}

impl AudioScheme for PlayingRing {
    fn set_params(&self, rate: u32, channels: u8) -> DeviceResult<(u32, u8)> {
        Ok((rate, channels))
    }
    fn params(&self) -> (u32, u8) {
        (48_000, 2)
    }
    fn write(&self, pcm: &[u8]) -> DeviceResult<usize> {
        let queued = self.queued_now();
        let n = self.cap.saturating_sub(queued).min(pcm.len());
        self.queued.lock().0 += n;
        Ok(n)
    }
    fn free_bytes(&self) -> usize {
        self.cap.saturating_sub(self.queued_now())
    }
    fn buffer_bytes(&self) -> usize {
        self.cap
    }
    fn queued_bytes(&self) -> usize {
        self.looks.fetch_add(1, Ordering::Relaxed);
        self.queued_now()
    }
    fn is_playing(&self) -> bool {
        self.queued_now() > 0
    }
    fn reset(&self) -> DeviceResult {
        self.queued.lock().0 = 0;
        Ok(())
    }
}

/// Un PCM preparado (arranca con el primer marco) sobre un anillo de `frames` marcos (a 48 kHz).
fn prepared(frames: usize) -> (Arc<PlayingRing>, PcmDev) {
    let ring = Arc::new(PlayingRing::new(frames));
    let pcm = PcmDev::new(ring.clone(), 0);
    {
        let mut st = pcm.st.lock();
        st.state = STATE_PREPARED;
        st.buffer_size = frames as u64;
        st.stop_threshold = frames as u64;
        st.period_size = frames as u64 / 2;
        st.avail_min = frames as u64 / 2;
    }
    (ring, pcm)
}

#[test]
fn the_two_blocking_pcm_ioctls_are_told_apart_from_the_rest() {
    // _IOW('A', 0x50, struct snd_xferi): lo que manda alsa-lib.
    assert_eq!(
        PcmBlockingIoctl::of(0x4018_4150),
        Some(PcmBlockingIoctl::WriteiFrames)
    );
    // _IO('A', 0x44)
    assert_eq!(PcmBlockingIoctl::of(0x4144), Some(PcmBlockingIoctl::Drain));
    // PREPARE, START y DROP no esperan.
    assert_eq!(PcmBlockingIoctl::of(0x4140), None);
    assert_eq!(PcmBlockingIoctl::of(0x4142), None);
    assert_eq!(PcmBlockingIoctl::of(0x4143), None);
    // El mismo número en otra familia ('d', DRM) no es del PCM.
    assert_eq!(PcmBlockingIoctl::of(0xc018_6450), None);
}

#[test]
fn a_writer_sleeps_for_the_time_its_room_takes_to_play_within_bounds() {
    // 240 marcos a 48 kHz son 5 ms.
    assert_eq!(refill_wait_for(240, 48_000), Duration::from_millis(5));
    // Un periodo de 1024 marcos (21 ms) se corta al techo: el anillo nunca
    // se queda sin datos por dormir de más.
    assert_eq!(refill_wait_for(1024, 48_000), REFILL_WAIT_MAX);
    // Un marco se redondea al suelo, el backoff de la espera que gira.
    assert_eq!(refill_wait_for(1, 48_000), REFILL_WAIT_MIN);
    // Una frecuencia a cero no divide por cero.
    assert_eq!(refill_wait_for(240, 0), REFILL_WAIT_MAX);
}

#[test]
fn a_drain_sleeps_in_slices_of_what_is_left_to_play() {
    assert_eq!(drain_wait_for(480, 48_000), Duration::from_millis(10));
    assert_eq!(drain_wait_for(16_384, 48_000), DRAIN_WAIT_MAX);
    assert_eq!(drain_wait_for(1, 48_000), DRAIN_WAIT_MIN);
}

#[test]
fn a_blocking_write_bigger_than_the_buffer_completes_asleep() {
    // 10 ms de anillo y 30 ms de escritura: dos tercios esperan sitio.
    let (ring, pcm) = prepared(480);
    let samples = alloc::vec![0u8; 1440 * BYTES_PER_FRAME as usize];
    let mut xfer = SndXferI {
        result: 0,
        buf: samples.as_ptr() as usize as u64,
        frames: 1440,
    };
    let data = &mut xfer as *mut SndXferI as usize;
    async_std::task::block_on(pcm.writei_sleeping(data)).unwrap();
    assert_eq!(xfer.result, 1440);
    assert_eq!(pcm.st.lock().appl_ptr, 1440);
    // Girando eran miles de miradas en 20 ms; durmiendo, unas pocas por
    // cada medio buffer que se libera.
    let looks = ring.looks.load(Ordering::Relaxed);
    assert!(looks < 200, "the writer looked at the ring {} times", looks);
}

#[test]
fn a_drain_waits_out_the_queue_asleep_and_leaves_the_stream_set_up() {
    let (ring, pcm) = prepared(960);
    let samples = alloc::vec![0u8; 960 * BYTES_PER_FRAME as usize];
    let mut xfer = SndXferI {
        result: 0,
        buf: samples.as_ptr() as usize as u64,
        frames: 960,
    };
    pcm.writei(&mut xfer, OpenFlags::NON_BLOCK).unwrap();
    assert_eq!(xfer.result, 960);
    let start = kernel_hal::timer::timer_now();
    async_std::task::block_on(pcm.drain_sleeping()).unwrap();
    // 960 marcos son 20 ms: el drain no vuelve antes de que suenen.
    assert!(kernel_hal::timer::timer_now() - start >= Duration::from_millis(15));
    assert_eq!(ring.queued_now(), 0);
    let st = pcm.st.lock();
    assert_eq!((st.state, st.appl_ptr), (STATE_SETUP, 0));
    drop(st);
    let looks = ring.looks.load(Ordering::Relaxed);
    assert!(looks < 200, "the drain looked at the ring {} times", looks);
}

#[test]
fn a_drain_of_a_stopped_stream_returns_at_once() {
    let (_ring, pcm) = prepared(480);
    async_std::task::block_on(pcm.drain_sleeping()).unwrap();
    assert_eq!(pcm.st.lock().state, STATE_SETUP);
}
