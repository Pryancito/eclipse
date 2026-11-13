// Heads up! Before working on this file you should read, at least,
// the parts of RFC 1122 that discuss ARP.

use managed::ManagedMap;

use crate::time::{Duration, Instant};
use crate::wire::{EthernetAddress, IpAddress};

/// A cached neighbor.
///
/// A neighbor mapping translates from a protocol address to a hardware address,
/// and contains the timestamp past which the mapping should be discarded.
#[derive(Debug, Clone, Copy)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub struct Neighbor {
    hardware_addr: EthernetAddress,
    expires_at: Instant,
}

/// An answer to a neighbor cache lookup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub(crate) enum Answer {
    /// The neighbor address is in the cache and not expired.
    Found(EthernetAddress),
    /// The neighbor address is not in the cache, or has expired.
    NotFound,
    /// The neighbor address is not in the cache, or has expired,
    /// and a discovery request for it has been sent recently.
    RateLimited,
    /// The neighbor address is not in the cache, and the last
    /// `ABSENT_AFTER` requests for it, one after another, were not
    /// answered: what is for it has nowhere to go. `ask` says whether
    /// another request may be sent now, which is how it is found again.
    Absent { ask: bool },
}

impl Answer {
    /// Returns whether a valid address was found.
    pub(crate) fn found(&self) -> bool {
        match self {
            Answer::Found(_) => true,
            _ => false,
        }
    }
}

/// How many neighbors a cache remembers as asked for at a time. With more
/// than this many asked within `FORGET_TIME`, the one asked longest ago
/// is forgotten early, and may be asked again sooner: still at most one
/// request for each packet that had nowhere to go.
const ASKED: usize = 8;

/// How many requests in a row a neighbor leaves unanswered before it is
/// taken as absent. Linux asks three times, a second apart.
const ABSENT_AFTER: u8 = 3;

/// A neighbor a discovery request was sent for and that has not answered.
#[derive(Debug, Clone, Copy)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
struct Asked {
    protocol_addr: IpAddress,
    /// When the last request for it was sent.
    at: Instant,
    /// How many were sent, one after another, with no answer.
    times: u8,
}

/// A neighbor cache backed by a map.
///
/// # Examples
///
/// On systems with heap, this cache can be created with:
///
/// ```rust
/// use std::collections::BTreeMap;
/// use smoltcp::iface::NeighborCache;
/// let mut neighbor_cache = NeighborCache::new(BTreeMap::new());
/// ```
///
/// On systems without heap, use:
///
/// ```rust
/// use smoltcp::iface::NeighborCache;
/// let mut neighbor_cache_storage = [None; 8];
/// let mut neighbor_cache = NeighborCache::new(&mut neighbor_cache_storage[..]);
/// ```
#[derive(Debug)]
pub struct Cache<'a> {
    storage: ManagedMap<'a, IpAddress, Neighbor>,
    /// The neighbors a discovery request was sent for and that have not
    /// answered: no other request is sent for one until `SILENT_TIME`
    /// after the last, and one that leaves `ABSENT_AFTER` unanswered is
    /// absent until it answers or is left alone for `FORGET_TIME`.
    ///
    /// One time for the whole cache, which is what this was, made the
    /// request for one neighbor wait for the request for another: a
    /// packet for a neighbor nobody had asked for, less than
    /// `SILENT_TIME` after a request for any other, sent no request at
    /// all, and its socket then kept quiet for its own
    /// `DISCOVERY_SILENT_TIME` -- three seconds for the first packet to
    /// a host, because a moment earlier something had asked for the name
    /// server. And a neighbor that never answers, asked again every time
    /// its socket's silence ends, could hold the others back round after
    /// round.
    asked: [Option<Asked>; ASKED],
    #[cfg(any(feature = "std", feature = "alloc"))]
    gc_threshold: usize,
}

impl<'a> Cache<'a> {
    /// Minimum delay between discovery requests for one neighbor, in
    /// milliseconds.
    pub(crate) const SILENT_TIME: Duration = Duration::from_millis(1_000);

    /// How long after the last request for a neighbor that did not
    /// answer it is forgotten that it was asked, in milliseconds. A
    /// neighbor taken as absent is so for as long as something keeps
    /// asking for it, and this long after: the next packet for it starts
    /// again, and waits for an answer as the first did.
    pub(crate) const FORGET_TIME: Duration = Duration::from_millis(3_000);

    /// Neighbor entry lifetime, in milliseconds.
    pub(crate) const ENTRY_LIFETIME: Duration = Duration::from_millis(60_000);

    /// Default number of entries in the cache before GC kicks in
    #[cfg(any(feature = "std", feature = "alloc"))]
    pub(crate) const GC_THRESHOLD: usize = 1024;

    /// Create a cache. The backing storage is cleared upon creation.
    ///
    /// # Panics
    /// This function panics if `storage.len() == 0`.
    pub fn new<T>(storage: T) -> Cache<'a>
    where
        T: Into<ManagedMap<'a, IpAddress, Neighbor>>,
    {
        let mut storage = storage.into();
        storage.clear();

        Cache {
            storage,
            #[cfg(any(feature = "std", feature = "alloc"))]
            gc_threshold: Self::GC_THRESHOLD,
            asked: [None; ASKED],
        }
    }

    #[cfg(any(feature = "std", feature = "alloc"))]
    pub fn new_with_limit<T>(storage: T, gc_threshold: usize) -> Cache<'a>
    where
        T: Into<ManagedMap<'a, IpAddress, Neighbor>>,
    {
        let mut storage = storage.into();
        storage.clear();

        Cache {
            storage,
            gc_threshold,
            asked: [None; ASKED],
        }
    }

    pub fn fill(
        &mut self,
        protocol_addr: IpAddress,
        hardware_addr: EthernetAddress,
        timestamp: Instant,
    ) {
        debug_assert!(protocol_addr.is_unicast());
        debug_assert!(hardware_addr.is_unicast());

        // It answered.
        for asked in self.asked.iter_mut() {
            if matches!(asked, Some(asked) if asked.protocol_addr == protocol_addr) {
                *asked = None;
            }
        }

        #[cfg(any(feature = "std", feature = "alloc"))]
        let current_storage_size = self.storage.len();

        match self.storage {
            ManagedMap::Borrowed(_) => (),
            #[cfg(any(feature = "std", feature = "alloc"))]
            ManagedMap::Owned(ref mut map) => {
                if current_storage_size >= self.gc_threshold {
                    let new_btree_map = map
                        .iter_mut()
                        .map(|(key, value)| (*key, *value))
                        .filter(|(_, v)| timestamp < v.expires_at)
                        .collect();

                    *map = new_btree_map;
                }
            }
        };
        let neighbor = Neighbor {
            expires_at: timestamp + Self::ENTRY_LIFETIME,
            hardware_addr,
        };
        match self.storage.insert(protocol_addr, neighbor) {
            Ok(Some(old_neighbor)) => {
                if old_neighbor.hardware_addr != hardware_addr {
                    net_trace!(
                        "replaced {} => {} (was {})",
                        protocol_addr,
                        hardware_addr,
                        old_neighbor.hardware_addr
                    );
                }
            }
            Ok(None) => {
                net_trace!("filled {} => {} (was empty)", protocol_addr, hardware_addr);
            }
            Err((protocol_addr, neighbor)) => {
                // If we're going down this branch, it means that a fixed-size cache storage
                // is full, and we need to evict an entry.
                let old_protocol_addr = match self.storage {
                    ManagedMap::Borrowed(ref mut pairs) => {
                        pairs
                            .iter()
                            .min_by_key(|pair_opt| {
                                let (_protocol_addr, neighbor) = pair_opt.unwrap();
                                neighbor.expires_at
                            })
                            .expect("empty neighbor cache storage") // unwraps min_by_key
                            .unwrap() // unwraps pair
                            .0
                    }
                    // Owned maps can extend themselves.
                    #[cfg(any(feature = "std", feature = "alloc"))]
                    ManagedMap::Owned(_) => unreachable!(),
                };

                let _old_neighbor = self.storage.remove(&old_protocol_addr).unwrap();
                match self.storage.insert(protocol_addr, neighbor) {
                    Ok(None) => {
                        net_trace!(
                            "filled {} => {} (evicted {} => {})",
                            protocol_addr,
                            hardware_addr,
                            old_protocol_addr,
                            _old_neighbor.hardware_addr
                        );
                    }
                    // We've covered everything else above.
                    _ => unreachable!(),
                }
            }
        }
    }

    pub(crate) fn lookup(&self, protocol_addr: &IpAddress, timestamp: Instant) -> Answer {
        if protocol_addr.is_broadcast() {
            return Answer::Found(EthernetAddress::BROADCAST);
        }

        if let Some(&Neighbor {
            expires_at,
            hardware_addr,
        }) = self.storage.get(protocol_addr)
        {
            if timestamp < expires_at {
                return Answer::Found(hardware_addr);
            }
        }

        let asked = self.asked.iter().flatten().find(|asked| {
            asked.protocol_addr == *protocol_addr && timestamp < asked.at + Self::FORGET_TIME
        });
        let Some(asked) = asked else {
            return Answer::NotFound;
        };
        // The last request may still be answered.
        let waiting = timestamp < asked.at + Self::SILENT_TIME;
        if asked.times > ABSENT_AFTER || (asked.times == ABSENT_AFTER && !waiting) {
            Answer::Absent { ask: !waiting }
        } else if waiting {
            Answer::RateLimited
        } else {
            Answer::NotFound
        }
    }

    /// A discovery request for `protocol_addr` was sent at `timestamp`:
    /// no other is sent for it until `SILENT_TIME` has passed. A request
    /// for any other neighbor is not held back by it.
    pub(crate) fn limit_rate(&mut self, protocol_addr: &IpAddress, timestamp: Instant) {
        let forgotten = |asked: &Asked| timestamp >= asked.at + Self::FORGET_TIME;
        // The place of this neighbor, or else one that is free, or else
        // the one asked longest ago, which is forgotten if any is.
        let own = self.asked.iter().position(
            |asked| matches!(asked, Some(asked) if asked.protocol_addr == *protocol_addr),
        );
        let free = || self.asked.iter().position(Option::is_none);
        let first = || {
            let places = self.asked.iter().enumerate();
            places
                .min_by_key(|(_, asked)| asked.map(|asked| asked.at))
                .map(|(at, _)| at)
        };
        if let Some(at) = own.or_else(free).or_else(first) {
            // One more of a run, or the first of one.
            let times = match self.asked[at] {
                Some(asked) if asked.protocol_addr == *protocol_addr && !forgotten(&asked) => {
                    asked.times.saturating_add(1)
                }
                _ => 1,
            };
            self.asked[at] = Some(Asked {
                protocol_addr: *protocol_addr,
                at: timestamp,
                times,
            });
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::wire::ip::test::{MOCK_IP_ADDR_1, MOCK_IP_ADDR_2, MOCK_IP_ADDR_3, MOCK_IP_ADDR_4};
    use std::collections::BTreeMap;

    const HADDR_A: EthernetAddress = EthernetAddress([0, 0, 0, 0, 0, 1]);
    const HADDR_B: EthernetAddress = EthernetAddress([0, 0, 0, 0, 0, 2]);
    const HADDR_C: EthernetAddress = EthernetAddress([0, 0, 0, 0, 0, 3]);
    const HADDR_D: EthernetAddress = EthernetAddress([0, 0, 0, 0, 0, 4]);

    #[test]
    fn test_fill() {
        let mut cache_storage = [Default::default(); 3];
        let mut cache = Cache::new(&mut cache_storage[..]);

        assert!(!cache
            .lookup(&MOCK_IP_ADDR_1, Instant::from_millis(0))
            .found());
        assert!(!cache
            .lookup(&MOCK_IP_ADDR_2, Instant::from_millis(0))
            .found());

        cache.fill(MOCK_IP_ADDR_1, HADDR_A, Instant::from_millis(0));
        assert_eq!(
            cache.lookup(&MOCK_IP_ADDR_1, Instant::from_millis(0)),
            Answer::Found(HADDR_A)
        );
        assert!(!cache
            .lookup(&MOCK_IP_ADDR_2, Instant::from_millis(0))
            .found());
        assert!(!cache
            .lookup(
                &MOCK_IP_ADDR_1,
                Instant::from_millis(0) + Cache::ENTRY_LIFETIME * 2
            )
            .found(),);

        cache.fill(MOCK_IP_ADDR_1, HADDR_A, Instant::from_millis(0));
        assert!(!cache
            .lookup(&MOCK_IP_ADDR_2, Instant::from_millis(0))
            .found());
    }

    #[test]
    fn test_expire() {
        let mut cache_storage = [Default::default(); 3];
        let mut cache = Cache::new(&mut cache_storage[..]);

        cache.fill(MOCK_IP_ADDR_1, HADDR_A, Instant::from_millis(0));
        assert_eq!(
            cache.lookup(&MOCK_IP_ADDR_1, Instant::from_millis(0)),
            Answer::Found(HADDR_A)
        );
        assert!(!cache
            .lookup(
                &MOCK_IP_ADDR_1,
                Instant::from_millis(0) + Cache::ENTRY_LIFETIME * 2
            )
            .found(),);
    }

    #[test]
    fn test_replace() {
        let mut cache_storage = [Default::default(); 3];
        let mut cache = Cache::new(&mut cache_storage[..]);

        cache.fill(MOCK_IP_ADDR_1, HADDR_A, Instant::from_millis(0));
        assert_eq!(
            cache.lookup(&MOCK_IP_ADDR_1, Instant::from_millis(0)),
            Answer::Found(HADDR_A)
        );
        cache.fill(MOCK_IP_ADDR_1, HADDR_B, Instant::from_millis(0));
        assert_eq!(
            cache.lookup(&MOCK_IP_ADDR_1, Instant::from_millis(0)),
            Answer::Found(HADDR_B)
        );
    }

    #[test]
    fn test_cache_gc() {
        let mut cache = Cache::new_with_limit(BTreeMap::new(), 2);
        cache.fill(MOCK_IP_ADDR_1, HADDR_A, Instant::from_millis(100));
        cache.fill(MOCK_IP_ADDR_2, HADDR_B, Instant::from_millis(50));
        // Adding third item after the expiration of the previous
        // two should garbage collect
        cache.fill(
            MOCK_IP_ADDR_3,
            HADDR_C,
            Instant::from_millis(50) + Cache::ENTRY_LIFETIME * 2,
        );

        assert_eq!(cache.storage.len(), 1);
        assert_eq!(
            cache.lookup(
                &MOCK_IP_ADDR_3,
                Instant::from_millis(50) + Cache::ENTRY_LIFETIME * 2
            ),
            Answer::Found(HADDR_C)
        );
    }

    #[test]
    fn test_evict() {
        let mut cache_storage = [Default::default(); 3];
        let mut cache = Cache::new(&mut cache_storage[..]);

        cache.fill(MOCK_IP_ADDR_1, HADDR_A, Instant::from_millis(100));
        cache.fill(MOCK_IP_ADDR_2, HADDR_B, Instant::from_millis(50));
        cache.fill(MOCK_IP_ADDR_3, HADDR_C, Instant::from_millis(200));
        assert_eq!(
            cache.lookup(&MOCK_IP_ADDR_2, Instant::from_millis(1000)),
            Answer::Found(HADDR_B)
        );
        assert!(!cache
            .lookup(&MOCK_IP_ADDR_4, Instant::from_millis(1000))
            .found());

        cache.fill(MOCK_IP_ADDR_4, HADDR_D, Instant::from_millis(300));
        assert!(!cache
            .lookup(&MOCK_IP_ADDR_2, Instant::from_millis(1000))
            .found());
        assert_eq!(
            cache.lookup(&MOCK_IP_ADDR_4, Instant::from_millis(1000)),
            Answer::Found(HADDR_D)
        );
    }

    #[test]
    fn test_hush() {
        let mut cache_storage = [Default::default(); 3];
        let mut cache = Cache::new(&mut cache_storage[..]);

        assert_eq!(
            cache.lookup(&MOCK_IP_ADDR_1, Instant::from_millis(0)),
            Answer::NotFound
        );

        cache.limit_rate(&MOCK_IP_ADDR_1, Instant::from_millis(0));
        assert_eq!(
            cache.lookup(&MOCK_IP_ADDR_1, Instant::from_millis(100)),
            Answer::RateLimited
        );
        assert_eq!(
            cache.lookup(&MOCK_IP_ADDR_1, Instant::from_millis(2000)),
            Answer::NotFound
        );
    }

    /// A request for one neighbor does not hold back the request for
    /// another: each has its own time.
    #[test]
    fn test_hush_is_per_neighbor() {
        let mut cache_storage = [Default::default(); 3];
        let mut cache = Cache::new(&mut cache_storage[..]);

        cache.limit_rate(&MOCK_IP_ADDR_1, Instant::from_millis(0));
        assert_eq!(
            cache.lookup(&MOCK_IP_ADDR_1, Instant::from_millis(200)),
            Answer::RateLimited
        );
        // Nobody asked for this one.
        assert_eq!(
            cache.lookup(&MOCK_IP_ADDR_2, Instant::from_millis(200)),
            Answer::NotFound
        );
        cache.limit_rate(&MOCK_IP_ADDR_2, Instant::from_millis(200));
        assert_eq!(
            cache.lookup(&MOCK_IP_ADDR_2, Instant::from_millis(999)),
            Answer::RateLimited
        );
        // Each until its own second has passed.
        assert_eq!(
            cache.lookup(&MOCK_IP_ADDR_1, Instant::from_millis(1000)),
            Answer::NotFound
        );
        assert_eq!(
            cache.lookup(&MOCK_IP_ADDR_2, Instant::from_millis(1000)),
            Answer::RateLimited
        );
        assert_eq!(
            cache.lookup(&MOCK_IP_ADDR_2, Instant::from_millis(1200)),
            Answer::NotFound
        );
        // Asked again, its time starts again, in the place it had.
        cache.limit_rate(&MOCK_IP_ADDR_1, Instant::from_millis(1500));
        assert_eq!(
            cache.lookup(&MOCK_IP_ADDR_1, Instant::from_millis(2400)),
            Answer::RateLimited
        );
        assert_eq!(
            cache.lookup(&MOCK_IP_ADDR_1, Instant::from_millis(2500)),
            Answer::NotFound
        );
        // One that answered is found, asked for or not.
        cache.fill(MOCK_IP_ADDR_1, HADDR_A, Instant::from_millis(2500));
        cache.limit_rate(&MOCK_IP_ADDR_1, Instant::from_millis(2500));
        assert_eq!(
            cache.lookup(&MOCK_IP_ADDR_1, Instant::from_millis(2600)),
            Answer::Found(HADDR_A)
        );
    }

    /// More neighbors asked for at once than are remembered: the one
    /// asked first is the one forgotten.
    #[test]
    fn test_hush_forgets_the_oldest() {
        let mut cache_storage = [Default::default(); 3];
        let mut cache = Cache::new(&mut cache_storage[..]);
        let neighbor = |n: usize| IpAddress::v4(10, 0, 0, n as u8 + 1);

        for n in 0..ASKED {
            cache.limit_rate(&neighbor(n), Instant::from_millis(n as i64));
        }
        for n in 0..ASKED {
            assert_eq!(
                cache.lookup(&neighbor(n), Instant::from_millis(100)),
                Answer::RateLimited
            );
        }
        cache.limit_rate(&neighbor(ASKED), Instant::from_millis(100));
        assert_eq!(
            cache.lookup(&neighbor(0), Instant::from_millis(101)),
            Answer::NotFound
        );
        for n in 1..=ASKED {
            assert_eq!(
                cache.lookup(&neighbor(n), Instant::from_millis(101)),
                Answer::RateLimited
            );
        }
    }

    /// A neighbor that leaves three requests in a row unanswered is
    /// absent: for as long as it is asked for, and until it has been left
    /// alone long enough to be forgotten.
    #[test]
    fn test_absent_after_three_unanswered() {
        let mut cache_storage = [Default::default(); 3];
        let mut cache = Cache::new(&mut cache_storage[..]);
        let lookup = |cache: &Cache, addr, ms| cache.lookup(&addr, Instant::from_millis(ms));

        for at in [0, 1000, 2000] {
            assert_eq!(lookup(&cache, MOCK_IP_ADDR_1, at), Answer::NotFound);
            cache.limit_rate(&MOCK_IP_ADDR_1, Instant::from_millis(at));
            // Each may still be answered, the third too.
            assert_eq!(
                lookup(&cache, MOCK_IP_ADDR_1, at + 999),
                Answer::RateLimited
            );
        }
        assert_eq!(
            lookup(&cache, MOCK_IP_ADDR_1, 3000),
            Answer::Absent { ask: true }
        );
        // Asked again, once a second, which is how it would be found.
        cache.limit_rate(&MOCK_IP_ADDR_1, Instant::from_millis(3000));
        assert_eq!(
            lookup(&cache, MOCK_IP_ADDR_1, 3500),
            Answer::Absent { ask: false }
        );
        assert_eq!(
            lookup(&cache, MOCK_IP_ADDR_1, 4000),
            Answer::Absent { ask: true }
        );
        // Nobody else is absent for it.
        assert_eq!(lookup(&cache, MOCK_IP_ADDR_2, 3500), Answer::NotFound);
        // Left alone, it is forgotten that it was asked: it starts again.
        assert_eq!(
            lookup(&cache, MOCK_IP_ADDR_1, 5999),
            Answer::Absent { ask: true }
        );
        assert_eq!(lookup(&cache, MOCK_IP_ADDR_1, 6000), Answer::NotFound);
        cache.limit_rate(&MOCK_IP_ADDR_1, Instant::from_millis(6000));
        assert_eq!(lookup(&cache, MOCK_IP_ADDR_1, 6500), Answer::RateLimited);
        assert_eq!(lookup(&cache, MOCK_IP_ADDR_1, 7000), Answer::NotFound);
    }

    /// One that answers is no longer absent, nor once its entry expires.
    #[test]
    fn test_absent_until_it_answers() {
        let mut cache_storage = [Default::default(); 3];
        let mut cache = Cache::new(&mut cache_storage[..]);

        for at in [0, 1000, 2000, 3000] {
            cache.limit_rate(&MOCK_IP_ADDR_1, Instant::from_millis(at));
        }
        assert_eq!(
            cache.lookup(&MOCK_IP_ADDR_1, Instant::from_millis(3100)),
            Answer::Absent { ask: false }
        );
        cache.fill(MOCK_IP_ADDR_1, HADDR_A, Instant::from_millis(3200));
        assert_eq!(
            cache.lookup(&MOCK_IP_ADDR_1, Instant::from_millis(3300)),
            Answer::Found(HADDR_A)
        );
        let expired = Instant::from_millis(3200) + Cache::ENTRY_LIFETIME;
        assert_eq!(cache.lookup(&MOCK_IP_ADDR_1, expired), Answer::NotFound);
    }
}
