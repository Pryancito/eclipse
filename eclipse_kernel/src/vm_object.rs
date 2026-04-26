//! VMObject system following FreeBSD design.
//!
//! A VMObject represents a source of data for a memory range (anonymous, file-backed, etc.).

use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use spin::Mutex;

#[derive(Debug, Clone)]
pub enum VMObjectType {
    /// Anonymous memory, lazily allocated and zero-filled.
    Anonymous,
    /// Memory backed by a physical address (e.g., Framebuffer, MMIO).
    Physical {
        phys_base: u64,
    },
    /// Memory backed by a file via a scheme.
    File {
        scheme_id: usize,
        resource_id: usize,
        offset: u64,
    },
}

#[derive(Debug)]
pub struct VMObject {
    pub obj_type: VMObjectType,
    pub size: u64,
    /// Reference count for sharing (e.g., across fork).
    pub refcount: usize,
    /// Pages already allocated for this object (page_index -> phys_addr).
    pub pages: BTreeMap<u64, u64>,
}

impl VMObject {
    pub fn new_anonymous(size: u64) -> Arc<Mutex<Self>> {
        Arc::new(Mutex::new(Self {
            obj_type: VMObjectType::Anonymous,
            size,
            refcount: 1,
            pages: BTreeMap::new(),
        }))
    }

    pub fn new_physical(phys_base: u64, size: u64) -> Arc<Mutex<Self>> {
        Arc::new(Mutex::new(Self {
            obj_type: VMObjectType::Physical { phys_base },
            size,
            refcount: 1,
            pages: BTreeMap::new(), // Not used for physical
        }))
    }

    pub fn new_file(scheme_id: usize, resource_id: usize, offset: u64, size: u64) -> Arc<Mutex<Self>> {
        Arc::new(Mutex::new(Self {
            obj_type: VMObjectType::File { scheme_id, resource_id, offset },
            size,
            refcount: 1,
            pages: BTreeMap::new(),
        }))
    }

    /// Creates a copy of the VMObject for a fork.
    /// If it's a private mapping, we copy the pages map and increment refcounts.
    pub fn clone_for_fork(&self) -> Arc<Mutex<Self>> {
        let mut new_pages = BTreeMap::new();
        for (&idx, &phys) in self.pages.iter() {
            crate::memory::frame_info::increment_refcount(phys);
            new_pages.insert(idx, phys);
        }

        Arc::new(Mutex::new(Self {
            obj_type: self.obj_type.clone(),
            size: self.size,
            refcount: 1,
            pages: new_pages,
        }))
    }
}
