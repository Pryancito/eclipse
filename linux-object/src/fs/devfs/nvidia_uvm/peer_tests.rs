//! What `cuCtxEnablePeerAccess` asks of UVM: peer access between two GPUs
//! of the VA space, when the RM says they have it.

use super::mapping_tests::*;
use super::tests::*;
use super::*;
use alloc::vec;

/// The RM's caps for a pair it gives peer access over PCIe BAR1.
const OVER_BAR1: nvctl::P2pCaps = nvctl::P2pCaps {
    read: 0,
    write: 0,
    atomics: 5,
    connectivity: 2,
};

fn peer_call(process: &Process, cmd: u32, a: &Uuid, b: &Uuid) -> u32 {
    let mut params = vec![0u8; peer::PEER_SIZE];
    params[..16].copy_from_slice(a);
    params[16..32].copy_from_slice(b);
    process.call(cmd, &mut params)
}

fn enable(process: &Process, a: &Uuid, b: &Uuid) -> u32 {
    peer_call(process, cmd::ENABLE_PEER_ACCESS, a, b)
}

fn disable(process: &Process, a: &Uuid, b: &Uuid) -> u32 {
    peer_call(process, cmd::DISABLE_PEER_ACCESS, a, b)
}

/// A process whose RM gives GPU0 and GPU1 peer access over PCIe.
fn with_peers() -> Process {
    let process = Process::new();
    process.rm.state().p2p.insert((0, 1), Ok(OVER_BAR1));
    process
}

/// The pair is enabled once, either way round, and disabled once.
#[test]
fn a_pair_the_rm_allows_is_enabled_once_and_disabled_once() {
    let process = with_peers();
    assert_eq!(enable(&process, &GPU0, &GPU1), NV_OK);
    assert!(process.rm.state().p2p_asked.contains(&(0, 1)));
    assert!(process
        .uvm
        .va_space
        .lock()
        .peers
        .contains(&peer::pair(&GPU0, &GPU1)));
    assert_eq!(enable(&process, &GPU1, &GPU0), NV_ERR_INVALID_DEVICE);
    assert_eq!(disable(&process, &GPU1, &GPU0), NV_OK);
    assert_eq!(disable(&process, &GPU0, &GPU1), NV_ERR_INVALID_DEVICE);
    assert_eq!(enable(&process, &GPU1, &GPU0), NV_OK);
}

/// A pair the RM gives no peer access, or only over NVLink (which UVM
/// enables on its own), is refused as `uvm_gpu_retain_pcie_peer_access`
/// refuses it; an RM that fails says why.
#[test]
fn a_pair_without_pcie_peer_access_is_refused() {
    let process = Process::new();
    assert_eq!(enable(&process, &GPU0, &GPU1), NV_ERR_INVALID_DEVICE);
    let nvlink = nvctl::P2pCaps {
        connectivity: 3,
        ..OVER_BAR1
    };
    process.rm.state().p2p.insert((0, 1), Ok(nvlink));
    assert_eq!(enable(&process, &GPU0, &GPU1), NV_ERR_INVALID_DEVICE);
    process.rm.state().p2p.insert((0, 1), Err(0x40));
    assert_eq!(enable(&process, &GPU0, &GPU1), 0x40);
    assert!(process.uvm.va_space.lock().peers.is_empty());
}

/// Both GPUs must be registered here, and be two.
#[test]
fn both_gpus_must_be_registered_and_different() {
    let process = with_peers();
    assert_eq!(enable(&process, &GPU0, &GPU0), NV_ERR_INVALID_DEVICE);
    assert_eq!(enable(&process, &GPU0, &[0x30; 16]), NV_ERR_INVALID_DEVICE);
    assert_eq!(disable(&process, &GPU0, &GPU0), NV_ERR_INVALID_DEVICE);
    assert_eq!(unregister_gpu(&process.uvm, &GPU1), NV_OK);
    assert_eq!(enable(&process, &GPU0, &GPU1), NV_ERR_INVALID_DEVICE);
}

/// Unregistering a GPU disables its pairs.
#[test]
fn unregistering_a_gpu_disables_its_peers() {
    let process = with_peers();
    assert_eq!(enable(&process, &GPU0, &GPU1), NV_OK);
    assert_eq!(unregister_gpu(&process.uvm, &GPU1), NV_OK);
    assert!(process.uvm.va_space.lock().peers.is_empty());
    assert_eq!(register_gpu(&process.uvm, &GPU1), NV_OK);
    assert_eq!(disable(&process, &GPU0, &GPU1), NV_ERR_INVALID_DEVICE);
}

/// The log names the RM's answer as nvidia-smi topo and the SDK do.
#[test]
fn the_log_names_the_caps() {
    assert_eq!(peer::status_name(0), "OK");
    assert_eq!(peer::status_name(5), "NOT_SUPPORTED");
    assert_eq!(peer::status_name(2), "GPU_NOT_SUPPORTED");
    assert_eq!(peer::connectivity_name(2), "PCIe (BAR1)");
    assert_eq!(peer::connectivity_name(0), "none");
}
