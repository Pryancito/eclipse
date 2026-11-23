//! The free blocks of one buddy class, as a set ordered by address.
//!
//! Not in upstream 0.8, whose free lists are singly linked LIFO stacks. A
//! free there had to find its buddy in that stack to merge with it, and the
//! only way to find it was to walk the stack: O(free blocks of the class) on
//! every level of every merge, with the heap lock held and, in the Eclipse
//! kernel, interrupts off. On the host, with 100 000 free 4 KiB blocks whose
//! buddies were live, one free cost 14 ms; freeing half of 358 000 pages at
//! random did not finish in ten minutes.
//!
//! Here each class is a treap keyed by the block's address. The two links a
//! node needs live in the free block itself, like the stack's one link did,
//! so the set costs no memory beside the heap: what it does cost is the
//! smallest block, two words instead of one. A node's priority is a hash of
//! its address, computed when it is needed and never stored, so the shape of
//! the tree is a function of the set of addresses alone and its expected
//! depth is logarithmic in the number of blocks. Finding a buddy, inserting a
//! block and taking one out are all a walk from the root to one node.
//!
//! Ordered by address, the set also answers two questions a stack cannot:
//! which free block is the highest (where an allocation should go, so that
//! live blocks pile up at one end of the heap and the other end can empty
//! out) and which is the lowest (what to give back first).

use core::mem::size_of;
use core::ptr::addr_of_mut;

/// The two links of a free block, written over its first two words.
#[repr(C)]
struct Node {
    left: usize,
    right: usize,
}

/// Bytes a free block needs to hold its node: the heap's smallest block.
pub const NODE_SIZE: usize = size_of::<Node>();

#[inline(always)]
fn node(addr: usize) -> *mut Node {
    addr as *mut Node
}

/// The priority of the block at `addr`: higher is nearer the root.
///
/// SplitMix64's finaliser, a bijection, so two blocks never tie. A plain
/// multiplicative hash is not enough: the addresses here are multiples of
/// large powers of two, and their low bits, which such a hash leaves alone,
/// are all zero.
#[inline(always)]
fn prio(addr: usize) -> u64 {
    let mut z = addr as u64;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// A set of free blocks of one size, ordered by address.
#[derive(Clone, Copy)]
pub struct FreeSet {
    root: usize,
    len: usize,
}

unsafe impl Send for FreeSet {}

impl FreeSet {
    pub const fn new() -> Self {
        FreeSet { root: 0, len: 0 }
    }

    pub fn is_empty(&self) -> bool {
        self.root == 0
    }

    /// Blocks in the set.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Add the block at `x`.
    ///
    /// # Safety
    /// `x` is non-null, aligned to a word, at least [`NODE_SIZE`] bytes that
    /// nobody else uses, and not in the set already.
    pub unsafe fn insert(&mut self, x: usize) {
        let px = prio(x);
        let mut link: *mut usize = addr_of_mut!(self.root);
        while *link != 0 && prio(*link) > px {
            let n = *link;
            link = if x < n {
                addr_of_mut!((*node(n)).left)
            } else {
                addr_of_mut!((*node(n)).right)
            };
        }
        let (l, r) = split(*link, x);
        (*node(x)).left = l;
        (*node(x)).right = r;
        *link = x;
        self.len += 1;
    }

    /// Take the block at `x` out of the set; `false` if it is not in it.
    ///
    /// Only links of blocks in the set are followed, so asking about an
    /// address that is not a free block -- a buddy that is in use, which is
    /// what a free asks about all the time -- reads nothing of it.
    pub fn remove(&mut self, x: usize) -> bool {
        // SAFETY: every link followed is a member's, written by `insert`,
        // `split` or `merge`.
        unsafe {
            let mut link: *mut usize = addr_of_mut!(self.root);
            while *link != 0 {
                let n = *link;
                if n == x {
                    *link = merge((*node(n)).left, (*node(n)).right);
                    self.len -= 1;
                    return true;
                }
                link = if x < n {
                    addr_of_mut!((*node(n)).left)
                } else {
                    addr_of_mut!((*node(n)).right)
                };
            }
            false
        }
    }

    /// Take the block with the highest address out of the set.
    pub fn pop_highest(&mut self) -> Option<usize> {
        // SAFETY: as in `remove`. The highest node has no right child, so
        // its left subtree, whose priorities are all below its own, takes
        // its place as it is.
        unsafe {
            let mut link: *mut usize = addr_of_mut!(self.root);
            if *link == 0 {
                return None;
            }
            while (*node(*link)).right != 0 {
                link = addr_of_mut!((*node(*link)).right);
            }
            let n = *link;
            *link = (*node(n)).left;
            self.len -= 1;
            Some(n)
        }
    }

    /// The lowest address in the set at which `accept` agrees, taken out.
    pub fn take_lowest_where(&mut self, mut accept: impl FnMut(usize) -> bool) -> Option<usize> {
        // SAFETY: as in `remove`.
        let found = unsafe { first_where(self.root, &mut accept) }?;
        let taken = self.remove(found);
        debug_assert!(taken);
        Some(found)
    }

    /// Whether the block at `x` is in the set.
    pub fn contains(&self, x: usize) -> bool {
        // SAFETY: as in `remove`.
        unsafe {
            let mut n = self.root;
            while n != 0 {
                if n == x {
                    return true;
                }
                n = if x < n {
                    (*node(n)).left
                } else {
                    (*node(n)).right
                };
            }
            false
        }
    }

    /// Every block of the set, lowest address first.
    pub fn for_each(&self, mut f: impl FnMut(usize)) {
        // SAFETY: as in `remove`.
        unsafe { visit(self.root, &mut f) }
    }

    /// Depth of the deepest node, for the tests.
    #[cfg(test)]
    pub fn depth(&self) -> usize {
        unsafe fn d(t: usize) -> usize {
            if t == 0 {
                0
            } else {
                1 + core::cmp::max(d((*node(t)).left), d((*node(t)).right))
            }
        }
        unsafe { d(self.root) }
    }
}

/// Split the treap at `t` into the nodes below `x` and those above it.
unsafe fn split(mut t: usize, x: usize) -> (usize, usize) {
    let mut l = 0usize;
    let mut r = 0usize;
    let mut lt: *mut usize = addr_of_mut!(l);
    let mut rt: *mut usize = addr_of_mut!(r);
    while t != 0 {
        // A block that is already free is being freed again. Linking it a
        // second time would make the tree a cycle and every later walk of
        // this class endless; better to stop here, where the culprit is on
        // the stack.
        assert!(t != x, "buddy: the block at {x:#x} is freed twice");
        if t < x {
            *lt = t;
            lt = addr_of_mut!((*node(t)).right);
            t = (*node(t)).right;
        } else {
            *rt = t;
            rt = addr_of_mut!((*node(t)).left);
            t = (*node(t)).left;
        }
    }
    *lt = 0;
    *rt = 0;
    (l, r)
}

/// Join two treaps, every address of `a` below every address of `b`.
unsafe fn merge(mut a: usize, mut b: usize) -> usize {
    let mut root = 0usize;
    let mut link: *mut usize = addr_of_mut!(root);
    loop {
        if a == 0 {
            *link = b;
            break;
        }
        if b == 0 {
            *link = a;
            break;
        }
        if prio(a) > prio(b) {
            *link = a;
            link = addr_of_mut!((*node(a)).right);
            a = (*node(a)).right;
        } else {
            *link = b;
            link = addr_of_mut!((*node(b)).left);
            b = (*node(b)).left;
        }
    }
    root
}

unsafe fn first_where(t: usize, accept: &mut impl FnMut(usize) -> bool) -> Option<usize> {
    if t == 0 {
        return None;
    }
    if let Some(found) = first_where((*node(t)).left, accept) {
        return Some(found);
    }
    if accept(t) {
        return Some(t);
    }
    first_where((*node(t)).right, accept)
}

unsafe fn visit(t: usize, f: &mut impl FnMut(usize)) {
    if t != 0 {
        visit((*node(t)).left, f);
        f(t);
        visit((*node(t)).right, f);
    }
}
