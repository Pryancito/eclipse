use super::super::{
    alloc_os_event, cards, map_unrecorded, release, rm_escape, rm_escape_lending, rm_map_memory,
    rm_up,
};
use super::*;
use alloc::vec;
use core::sync::atomic::{AtomicUsize, Ordering};
use nvidia_rm_sys::types::{
    NV_ERR_INSUFFICIENT_RESOURCES, NV_ERR_INVALID_STATE, NV_ERR_NOT_READY, NV_ERR_NOT_SUPPORTED,
    NV_OK,
};

fn card(minor: u32, instance: Option<u32>) -> ServedCard {
    ServedCard {
        card: Card {
            minor,
            domain: 0,
            bus: 0x2d + minor as u8,
            slot: 0,
            device_id: 0x2684,
            reg_address: 0xfb00_0000,
            reg_size: 16 << 20,
            fb_address: 0xd000_0000_0000 + minor as u64,
            fb_size: 256 << 20,
            instance,
        },
        gpu_id: 0x2d00 + minor,
        uuid: instance.map(|i| [i as u8 + 1; GPU_UUID_LEN]),
    }
}

fn a_caller() -> Caller {
    Caller {
        os_info: 0xffff_8000_1234_0000,
        ctl: true,
        admin: false,
        gpu_mask: 0b10,
        pid: 4242,
        name: Caller::name_of(b"nvidia-smi"),
    }
}

#[test]
fn a_hello_says_the_rm_and_its_gpus() {
    let cards = vec![card(0, Some(0)), card(1, None)];
    let b = hello_of(true, &cards);
    assert_eq!(parse_hello(&b), Some((true, cards.clone())));
    assert_eq!(parse_hello(&hello_of(false, &[])), Some((false, vec![])));
}

#[test]
fn a_hello_that_is_not_one_is_refused() {
    let b = hello_of(true, &[card(0, Some(0))]);
    assert_eq!(parse_hello(&b[..b.len() - 1]), None, "short");
    let mut magic = b.clone();
    magic[0] ^= 1;
    assert_eq!(parse_hello(&magic), None, "not ours");
    let mut up = b.clone();
    up[8] = 2;
    assert_eq!(parse_hello(&up), None, "rm_up is a flag");
    let mut flag = b.clone();
    flag[HEAD_LEN + 12] = 7;
    assert_eq!(parse_hello(&flag), None, "instance is a flag");
    // A minor out of its place.
    let b = hello_of(true, &[card(1, Some(0))]);
    assert_eq!(parse_hello(&b), None);
    let many: Vec<ServedCard> = (0..=CARDS_MAX as u32).map(|m| card(m, None)).collect();
    assert_eq!(parse_hello(&hello_of(true, &many)), None, "too many");
}

#[test]
fn a_caller_goes_whole_and_its_name_always_ends() {
    let c = a_caller();
    let b = encode_caller(&c);
    let back = decode_caller(&b).unwrap();
    assert_eq!(
        (
            back.os_info,
            back.ctl,
            back.admin,
            back.gpu_mask,
            back.pid,
            back.name
        ),
        (c.os_info, c.ctl, c.admin, c.gpu_mask, c.pid, c.name)
    );
    let mut long = b;
    long[24..].fill(b'x');
    assert_eq!(decode_caller(&long).unwrap().name[CALLER_NAME_LEN - 1], 0);
    let mut bad = b;
    bad[8] = 2;
    assert!(decode_caller(&bad).is_none(), "ctl is a flag");
}

/// An RM that answers an escape by turning its payload around, and counts
/// what it was told.
#[derive(Default)]
struct Mirror {
    released: AtomicUsize,
    last_pid: AtomicUsize,
    /// What `map_memory` grants, and the last `why` it was told.
    grants: lock::Mutex<Option<MmapContext>>,
    unrecorded: AtomicUsize,
    /// The pages it was last lent, as it took them, while it keeps them;
    /// and whether it keeps the next ones.
    kept: lock::Mutex<Option<UserPages>>,
    keeps: core::sync::atomic::AtomicBool,
    lent: lock::Mutex<Option<(usize, Vec<u64>, bool)>>,
}

impl Rm for Mirror {
    fn escape(&self, cmd: u32, payload: &mut [u8], caller: &Caller) -> NV_STATUS {
        self.last_pid.store(caller.pid as usize, Ordering::SeqCst);
        payload.reverse();
        cmd + 1
    }

    fn release(&self, os_info: usize) {
        self.released.store(os_info, Ordering::SeqCst);
    }

    fn escape_lending(
        &self,
        cmd: u32,
        payload: &mut [u8],
        _caller: &Caller,
        pages: UserPages,
    ) -> NV_STATUS {
        *self.lent.lock() = Some((pages.va, pages.phys.clone(), pages.writable));
        payload.reverse();
        if self.keeps.load(Ordering::SeqCst) {
            *self.kept.lock() = Some(pages);
        }
        cmd + 2
    }

    fn map_memory(&self, payload: &mut [u8], _caller: &Caller) -> (NV_STATUS, Option<MmapContext>) {
        payload.fill(0xee);
        (NV_OK, *self.grants.lock())
    }

    fn map_unrecorded(&self, payload: &mut [u8], _caller: &Caller, why: NV_STATUS) {
        payload[0] = 0x55;
        self.unrecorded.store(why as usize, Ordering::SeqCst);
    }
}

fn system(start: u64, size: u64) -> MmapContext {
    MmapContext::from_raw(&RawMmapContext {
        valid: 1,
        ctl: 1,
        gpu_instance: 0,
        memory_type: 0,
        caching: 0,
        prot: 2,
        start,
        size,
    })
    .unwrap()
}

#[test]
fn the_server_answers_an_escape_and_a_release() {
    let rm = Mirror::default();
    let mut msg = encode_caller(&a_caller()).to_vec();
    msg.extend_from_slice(&[1, 2, 3]);
    let mut out = Vec::new();
    assert_eq!(answer(&rm, op::RM_ESCAPE, 0x2a, &msg, &mut out), Ok(0x2b));
    assert_eq!(out, [3, 2, 1]);
    assert_eq!(rm.last_pid.load(Ordering::SeqCst), 4242);
    assert_eq!(answer(&rm, op::RELEASE, 0x77, &[], &mut out), Ok(0));
    assert_eq!(rm.released.load(Ordering::SeqCst), 0x77);
    assert_eq!(
        answer(&rm, op::RM_ESCAPE, 0x2a, &msg[..CALLER_LEN - 1], &mut out),
        Err(EINVAL)
    );
    assert_eq!(
        answer(&rm, op::RM_ESCAPE, 1 << 40, &msg, &mut out),
        Err(EINVAL)
    );
    assert_eq!(answer(&rm, 7, 0, &[], &mut out), Err(ENOSYS));
}

/// What the loader would say: pins at 0x1000_0000.., a BAR at
/// 0xe000_0000..; pages lent as they are, counted in [`LENT`], those given
/// back in [`GIVEN`].
fn server() -> Server {
    Server {
        owns: owns_of(),
        lend: Arc::new(|pages| {
            if pages.phys.contains(&0) {
                return None;
            }
            LENT.fetch_add(1, Ordering::SeqCst);
            Some(Box::new(Counted))
        }),
        id: SERVER,
    }
}

const SERVER: u64 = 0x5e;
static LENT: AtomicUsize = AtomicUsize::new(0);
static GIVEN: AtomicUsize = AtomicUsize::new(0);

struct Counted;

impl Loan for Counted {
    fn give_back(self: Box<Self>) {
        GIVEN.fetch_add(1, Ordering::SeqCst);
    }
}

fn owns_of() -> Owns {
    Arc::new(|start, size| {
        let end = start + size;
        if start >= 0x1000_0000 && end <= 0x1010_0000 {
            Some(false)
        } else if start >= 0xe000_0000 && end <= 0xf000_0000 {
            Some(true)
        } else {
            None
        }
    })
}

/// The kernel's end of a link straight to [`answer`] on a [`Mirror`].
struct Direct(Mirror, AtomicUsize);

impl Link for Direct {
    fn call(&self, op: u32, a: u64, data: &[u8]) -> Result<(u64, Vec<u8>), i32> {
        let mut out = Vec::new();
        let word = answer(&self.0, op, a, data, &mut out)?;
        Ok((word, out))
    }

    fn tell(&self, op: u32, a: u64) {
        self.1.fetch_add(1, Ordering::SeqCst);
        let _ = answer(&self.0, op, a, &[], &mut Vec::new());
    }
}

/// With a process serving the RM, the device files' calls are its: the
/// escapes and closes go over the link, the GPUs are its, and what is not
/// carried yet is refused without entering this address space's RM (which
/// was never built). Once it is gone, there is no RM again.
#[test]
fn a_served_rm_is_the_one_the_device_files_reach() {
    let _g = crate::test_globals::LOCK.lock();
    assert!(!rm_up());
    let direct = Arc::new(Direct(Mirror::default(), AtomicUsize::new(0)));
    let link: Arc<dyn Link> = direct.clone();
    let served = vec![card(0, Some(0)), card(1, Some(3))];
    assert_eq!(
        serve(link.clone(), &hello_of(true, &served), server()),
        Ok(served.clone())
    );
    assert!(rm_up());
    let listed = cards();
    assert_eq!(listed.len(), 2);
    assert_eq!(listed[1].gpu_id(), 0x2d01, "the process's RM's id");
    assert_eq!(listed[1].uuid(), Some([4; GPU_UUID_LEN]));

    let mut payload = [1u8, 2, 3, 4];
    assert_eq!(rm_escape(0x2a, &mut payload, &a_caller()), 0x2b);
    assert_eq!(payload, [4, 3, 2, 1]);
    release(0x99);
    assert_eq!(direct.0.released.load(Ordering::SeqCst), 0x99);
    assert_eq!(direct.1.load(Ordering::SeqCst), 1, "told, not asked");
    assert_eq!(alloc_os_event(1, 2, 3), NV_ERR_NOT_SUPPORTED);

    let other: Arc<dyn Link> = Arc::new(Direct(Mirror::default(), AtomicUsize::new(0)));
    assert_eq!(
        serve(other.clone(), &hello_of(true, &[]), server()),
        Err(Refused::Taken)
    );
    gone(&other);
    assert!(rm_up(), "only the server's own going ends it");

    gone(&link);
    assert!(!rm_up());
    assert_eq!(rm_escape(0x2a, &mut payload, &a_caller()), NV_ERR_NOT_READY);
    assert_eq!(alloc_os_event(1, 2, 3), NV_OK);
    let _ = nvidia_rm_sys::frontend::events::free_os_event(1, 3);
}

/// A process whose RM is not up yet serves no escape: they are refused as
/// with no RM.
#[test]
fn a_served_rm_that_is_down_takes_no_escape() {
    let _g = crate::test_globals::LOCK.lock();
    let direct = Arc::new(Direct(Mirror::default(), AtomicUsize::new(0)));
    let link: Arc<dyn Link> = direct.clone();
    assert!(serve(link.clone(), &hello_of(false, &[card(0, None)]), server()).is_ok());
    assert!(!rm_up());
    let mut payload = [1u8, 2];
    assert_eq!(rm_escape(0x2a, &mut payload, &a_caller()), NV_ERR_NOT_READY);
    assert_eq!(payload, [1, 2]);
    assert_eq!(cards().len(), 1);
    assert_eq!(cards()[0].gpu_id(), u32::MAX);
    gone(&link);
    assert_eq!(serve(link.clone(), b"no", server()), Err(Refused::Invalid));
}

/// A mapping the process's RM grants is the kernel's only if it is the
/// process's memory, of the kind it says: system memory it pinned, an
/// aperture in a BAR. Anything else is refused, and the RM told so.
#[test]
fn a_served_rm_maps_only_its_process_s_memory() {
    let _g = crate::test_globals::LOCK.lock();
    let direct = Arc::new(Direct(Mirror::default(), AtomicUsize::new(0)));
    let link: Arc<dyn Link> = direct.clone();
    serve(link.clone(), &hello_of(true, &[card(0, Some(0))]), server()).unwrap();
    let grant = |m: Option<MmapContext>| *direct.0.grants.lock() = m;

    let mut payload = [0u8; 56];
    grant(Some(system(0x1000_0000, 0x2000)));
    assert_eq!(
        rm_map_memory(&mut payload, &a_caller()),
        (NV_OK, Some(system(0x1000_0000, 0x2000)))
    );
    assert_eq!(payload, [0xee; 56], "the payload as the RM left it");

    grant(None);
    assert_eq!(rm_map_memory(&mut payload, &a_caller()), (NV_OK, None));

    // System memory it never pinned, and a BAR called system memory.
    for start in [0x2000_0000, 0xe000_0000] {
        grant(Some(system(start, 0x1000)));
        assert_eq!(
            rm_map_memory(&mut payload, &a_caller()),
            (NV_ERR_INVALID_STATE, None)
        );
        assert_eq!(payload[0], 0x55, "the RM was told");
        assert_eq!(
            direct.0.unrecorded.swap(0, Ordering::SeqCst),
            NV_ERR_INVALID_STATE as usize
        );
    }
    // An aperture in its BAR.
    let mut aperture = system(0xe000_0000, 0x1000).to_raw();
    aperture.ctl = 0;
    aperture.memory_type = 2;
    let aperture = MmapContext::from_raw(&aperture).unwrap();
    grant(Some(aperture));
    assert_eq!(
        rm_map_memory(&mut payload, &a_caller()),
        (NV_OK, Some(aperture))
    );

    map_unrecorded(&mut payload, &a_caller(), 0x77);
    assert_eq!(direct.0.unrecorded.load(Ordering::SeqCst), 0x77);
    gone(&link);
}

#[test]
fn a_mapping_goes_whole() {
    let m = system(0x1000_0000, 0x3000);
    assert_eq!(
        MmapContext::from_raw(&decode_mapping(&encode_mapping(Some(&m)))),
        Some(m)
    );
    assert_eq!(decode_mapping(&encode_mapping(None)).valid, 0);
}

/// The kernel's pinned pages, with a count of their unpinning.
fn pinned(va: usize, phys: Vec<u64>) -> UserPages {
    struct Unpinned;
    impl Drop for Unpinned {
        fn drop(&mut self) {
            UNPINNED.fetch_add(1, Ordering::SeqCst);
        }
    }
    UserPages {
        va,
        phys,
        writable: true,
        keep: Box::new(Unpinned),
    }
}

static UNPINNED: AtomicUsize = AtomicUsize::new(0);

/// The process gives a loan back as the gateway call would.
fn give_back_as_server(loan: u64) {
    let _ = give_back(SERVER, loan);
}

/// Pages lent with an escape reach the process's RM with their addresses;
/// they go back (granted, then unpinned) when its RM drops them, whether at
/// once or later, and are not given back for a process that is gone.
#[test]
fn pages_lent_to_a_served_rm_come_back_when_it_lets_them_go() {
    let _g = crate::test_globals::LOCK.lock();
    set_give_back(give_back_as_server);
    let counts = || {
        (
            LENT.load(Ordering::SeqCst),
            GIVEN.load(Ordering::SeqCst),
            UNPINNED.load(Ordering::SeqCst),
        )
    };
    let start = counts();
    let at = |d: (usize, usize, usize)| (start.0 + d.0, start.1 + d.1, start.2 + d.2);
    let direct = Arc::new(Direct(Mirror::default(), AtomicUsize::new(0)));
    let link: Arc<dyn Link> = direct.clone();
    serve(link.clone(), &hello_of(true, &[card(0, Some(0))]), server()).unwrap();

    // Untaken: back by the end of the call.
    let mut payload = [1u8, 2, 3];
    let pages = pinned(0x7000_0000, vec![0x1234_5000, 0x2000_0000]);
    assert_eq!(
        rm_escape_lending(0x2a, &mut payload, &a_caller(), Some(pages)),
        0x2c
    );
    assert_eq!(payload, [3, 2, 1]);
    assert_eq!(
        *direct.0.lent.lock(),
        Some((0x7000_0000, vec![0x1234_5000, 0x2000_0000], true))
    );
    assert_eq!(counts(), at((1, 1, 1)));

    // Taken, and let go later.
    direct.0.keeps.store(true, Ordering::SeqCst);
    let pages = pinned(0x7100_0000, vec![0x3000]);
    assert_eq!(
        rm_escape_lending(0x2a, &mut payload, &a_caller(), Some(pages)),
        0x2c
    );
    assert_eq!(counts(), at((2, 1, 1)), "still the process's");
    assert!(!give_back(SERVER + 1, 2), "only the server gives back");
    drop(direct.0.kept.lock().take());
    assert_eq!(counts(), at((2, 2, 2)));
    assert!(!give_back(SERVER, 2), "once");

    // None lent: a plain escape.
    assert_eq!(
        rm_escape_lending(0x2a, &mut payload, &a_caller(), None),
        0x2b
    );
    // Pages the GPUs cannot be granted, and more than a message holds.
    let refused = [vec![0x3000, 0], vec![0x3000; MESSAGE_MAX / 8]];
    for phys in refused {
        let pages = pinned(0x7200_0000, phys);
        assert_eq!(
            rm_escape_lending(0x2a, &mut payload, &a_caller(), Some(pages)),
            NV_ERR_INSUFFICIENT_RESOURCES
        );
    }
    assert_eq!(counts(), at((2, 2, 4)));

    // Kept when the process goes: unpinned by the kernel, never given back.
    let pages = pinned(0x7300_0000, vec![0x4000]);
    rm_escape_lending(0x2a, &mut payload, &a_caller(), Some(pages));
    gone(&link);
    assert_eq!(counts(), at((3, 2, 5)));
    let kept = direct.0.kept.lock().take();
    drop(kept);
    assert_eq!(counts(), at((3, 2, 5)), "nobody to give it back to");
}

/// A loan on the link goes whole, and one that is not is refused.
#[test]
fn a_loan_goes_whole() {
    let rm = Mirror::default();
    let pages = pinned(0x7000_0000, vec![0x1000, 0x5000]);
    let mut msg = encode_caller(&a_caller()).to_vec();
    msg.extend_from_slice(&encode_loan(9, &pages));
    for p in &pages.phys {
        msg.extend_from_slice(&p.to_le_bytes());
    }
    msg.extend_from_slice(&[7, 8]);
    let mut out = Vec::new();
    assert_eq!(answer(&rm, op::RM_ESCAPE_LENDING, 1, &msg, &mut out), Ok(3));
    assert_eq!(out, [8, 7]);
    assert_eq!(
        *rm.lent.lock(),
        Some((0x7000_0000, vec![0x1000, 0x5000], true))
    );
    let short = &msg[..CALLER_LEN + LOAN_LEN + 8];
    assert_eq!(
        answer(&rm, op::RM_ESCAPE_LENDING, 1, short, &mut out),
        Err(EINVAL)
    );
    let mut reserved = msg.clone();
    reserved[CALLER_LEN + 24] = 1;
    assert_eq!(
        answer(&rm, op::RM_ESCAPE_LENDING, 1, &reserved, &mut out),
        Err(EINVAL)
    );
}
