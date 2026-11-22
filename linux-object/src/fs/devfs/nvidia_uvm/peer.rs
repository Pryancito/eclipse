//! Peer access between two GPUs of a VA space (`cuCtxEnablePeerAccess`,
//! `cudaDeviceEnablePeerAccess`).
//!
//! libcuda asks the RM first: `cuDeviceCanAccessPeer` is the RM's answer to
//! NV0000_CTRL_CMD_SYSTEM_GET_P2P_CAPS, and only on a yes does
//! `cuCtxEnablePeerAccess` reach UVM_ENABLE_PEER_ACCESS. nvidia-uvm checks
//! the same caps (`nvGpuOpsGetP2PCaps`, kept per GPU pair when the GPUs are
//! registered) and enables the pair over PCIe only: an NVLink pair is
//! enabled on its own, and asking is NV_ERR_INVALID_DEVICE
//! (`uvm_gpu_retain_pcie_peer_access`).
//!
//! The RM's answer is logged the first time two GPUs are registered in one
//! VA space (cuInit registers every GPU), with the reason when it is a no:
//! the line that says why `cuDeviceCanAccessPeer` answers 0. On a GeForce
//! card over PCIe the RM (GSP-RM) answers no on Linux as well, unless the
//! `ForceP2P` registry key says otherwise (`nvidia.registry=ForceP2P=0x11`
//! here, `NVreg_RegistryDwords` there).
//!
//! Enabling the pair is recorded and nothing else: UVM's own peer mappings
//! are of the memory UVM migrates, and here managed memory stays in system
//! memory that every GPU maps already ([`super::managed`]). The peer
//! mappings of a GPU's own memory are libcuda's, made through the RM.

use super::*;

/// `UVM_ENABLE_PEER_ACCESS_PARAMS` and its DISABLE twin: `NvProcessorUuid
/// gpuUuidA`, `NvProcessorUuid gpuUuidB`, `NV_STATUS rmStatus`.
pub(super) const PEER_SIZE: usize = 36;
pub(super) const PEER_STATUS: usize = 32;

/// `P2P_CONNECTIVITY_PCIE_PROPRIETARY` and `P2P_CONNECTIVITY_PCIE_BAR1`
/// (p2p_caps.h): what `UVM_GPU_LINK_PCIE` is made of.
const CONNECTIVITY_PCIE_MAILBOX: u32 = 1;
const CONNECTIVITY_PCIE_BAR1: u32 = 2;

/// A GPU pair, the lower UUID first (`uvm_gpu_pair_index`).
pub(super) fn pair(a: &Uuid, b: &Uuid) -> (Uuid, Uuid) {
    if a <= b {
        (*a, *b)
    } else {
        (*b, *a)
    }
}

/// Whether the RM would give the pair peer access over PCIe.
fn over_pcie(caps: &nvctl::P2pCaps) -> bool {
    caps.supported()
        && matches!(
            caps.connectivity,
            CONNECTIVITY_PCIE_MAILBOX | CONNECTIVITY_PCIE_BAR1
        )
}

impl UvmOpen {
    /// The RM GPU instance of a GPU it brought up.
    fn instance_of(&self, uuid: &Uuid) -> Option<u32> {
        self.rm
            .gpus()
            .into_iter()
            .find(|(gpu, _)| gpu == uuid)
            .map(|(_, instance)| instance)
    }

    /// The RM's P2P caps of a pair of GPUs it brought up.
    fn p2p_caps_of(&self, a: &Uuid, b: &Uuid) -> core::result::Result<nvctl::P2pCaps, u32> {
        let (Some(ia), Some(ib)) = (self.instance_of(a), self.instance_of(b)) else {
            return Err(NV_ERR_INVALID_DEVICE);
        };
        self.rm.p2p_caps(ia, ib)
    }

    /// What `uvm_gpu_retain` learns of the peers of a GPU just registered:
    /// the RM's caps with every other GPU registered here, logged the first
    /// time each pair is seen this boot.
    pub(super) fn note_peers_of(&self, uuid: &Uuid) {
        let others: Vec<Uuid> = self
            .va_space
            .lock()
            .gpus
            .iter()
            .filter(|gpu| *gpu != uuid)
            .copied()
            .collect();
        for other in others {
            let key = pair(uuid, &other);
            if !LOGGED_PAIRS.lock().insert(key) {
                continue;
            }
            log_caps(&key.0, &key.1, self.p2p_caps_of(&key.0, &key.1));
        }
    }

    /// `uvm_api_enable_peer_access`: two different GPUs registered here,
    /// whose RM caps say peer access over PCIe, not enabled yet.
    pub(super) fn enable_peer_access(&self, a: &Uuid, b: &Uuid) -> u32 {
        {
            let va_space = self.va_space.lock();
            if a == b || !va_space.gpus.contains(a) || !va_space.gpus.contains(b) {
                return NV_ERR_INVALID_DEVICE;
            }
        }
        match self.p2p_caps_of(a, b) {
            Ok(caps) if over_pcie(&caps) => {}
            Ok(caps) => {
                peer_refused(a, b, Ok(caps));
                return NV_ERR_INVALID_DEVICE;
            }
            Err(status) => {
                peer_refused(a, b, Err(status));
                return status;
            }
        }
        let mut va_space = self.va_space.lock();
        // Unregistered by another thread meanwhile.
        if !va_space.gpus.contains(a) || !va_space.gpus.contains(b) {
            return NV_ERR_INVALID_DEVICE;
        }
        if !va_space.peers.insert(pair(a, b)) {
            return NV_ERR_INVALID_DEVICE;
        }
        first_time(
            &FIRST_PEERS,
            "UVM_ENABLE_PEER_ACCESS: the first GPU pair given peer access, over PCIe",
        );
        NV_OK
    }

    /// `uvm_api_disable_peer_access`: a pair enabled here.
    pub(super) fn disable_peer_access(&self, a: &Uuid, b: &Uuid) -> u32 {
        let mut va_space = self.va_space.lock();
        if a == b || !va_space.gpus.contains(a) || !va_space.gpus.contains(b) {
            return NV_ERR_INVALID_DEVICE;
        }
        if va_space.peers.remove(&pair(a, b)) {
            NV_OK
        } else {
            NV_ERR_INVALID_DEVICE
        }
    }
}

impl VaSpace {
    /// The pairs of `uuid` disabled, as unregistering it does
    /// (`disable_peers` from `va_space_unregister_gpu`).
    pub(super) fn drop_peers_of(&mut self, uuid: &Uuid) {
        self.peers.retain(|(a, b)| a != uuid && b != uuid);
    }
}

/// The GPU pairs whose caps were logged this boot.
static LOGGED_PAIRS: spin::Mutex<BTreeSet<(Uuid, Uuid)>> = spin::Mutex::new(BTreeSet::new());
static FIRST_PEERS: AtomicBool = AtomicBool::new(false);

/// `NV0000_P2P_CAPS_STATUS_*` by name.
pub(super) fn status_name(status: u8) -> &'static str {
    match status {
        0 => "OK",
        1 => "CHIPSET_NOT_SUPPORTED",
        2 => "GPU_NOT_SUPPORTED",
        3 => "IOH_TOPOLOGY_NOT_SUPPORTED",
        4 => "DISABLED_BY_REGKEY",
        5 => "NOT_SUPPORTED",
        _ => "?",
    }
}

/// `P2P_CONNECTIVITY` by name.
pub(super) fn connectivity_name(connectivity: u32) -> &'static str {
    match connectivity {
        0 => "none",
        CONNECTIVITY_PCIE_MAILBOX => "PCIe (mailbox)",
        CONNECTIVITY_PCIE_BAR1 => "PCIe (BAR1)",
        3 => "NVLink",
        4 => "NVLink (indirect)",
        5 => "C2C",
        _ => "?",
    }
}

fn log_caps(a: &Uuid, b: &Uuid, caps: core::result::Result<nvctl::P2pCaps, u32>) {
    match caps {
        Ok(caps) if caps.supported() => zcore_drivers::klog_warn!(
            "[nvidia-uvm] P2P {} <-> {}: yes, over {} (atomics {}); cuDeviceCanAccessPeer answers 1",
            ShowUuid(a),
            ShowUuid(b),
            connectivity_name(caps.connectivity),
            status_name(caps.atomics)
        ),
        Ok(caps) => zcore_drivers::klog_warn!(
            "[nvidia-uvm] P2P {} <-> {}: no (reads {}, writes {}); cuDeviceCanAccessPeer answers 0 and peer copies go through system memory. nvidia.registry=ForceP2P=0x11 makes the RM try anyway, as NVreg_RegistryDwords does on Linux",
            ShowUuid(a),
            ShowUuid(b),
            status_name(caps.read),
            status_name(caps.write)
        ),
        Err(status) => zcore_drivers::klog_warn!(
            "[nvidia-uvm] P2P {} <-> {}: the RM did not say: status {:#x}",
            ShowUuid(a),
            ShowUuid(b),
            status
        ),
    }
}

/// Log lines the first time UVM_ENABLE_PEER_ACCESS is refused for the RM's
/// caps, with those caps.
fn peer_refused(a: &Uuid, b: &Uuid, caps: core::result::Result<nvctl::P2pCaps, u32>) {
    static SAID: AtomicBool = AtomicBool::new(false);
    if SAID.swap(true, Ordering::Relaxed) {
        return;
    }
    zcore_drivers::klog_warn!(
        "[nvidia-uvm] UVM_ENABLE_PEER_ACCESS refused: NV_ERR_INVALID_DEVICE unless the RM failed"
    );
    log_caps(a, b, caps);
}
