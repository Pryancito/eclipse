use std::process;

use std::arch::asm;

use rand_chacha::ChaCha20Rng;
use rand_core::RngCore;

pub const MODE_PERM: u16 = 0x0FFF;
pub const MODE_EXEC: u16 = 0o1;
pub const MODE_WRITE: u16 = 0o2;
pub const MODE_READ: u16 = 0o4;

#[cfg(target_arch = "x86_64")]
use raw_cpuid::CpuId;

use redox_scheme::{RequestKind, SchemeMut, SignalBehavior, Socket, V2};
use syscall::data::Stat;
use syscall::flag::EventFlags;
use syscall::{
    Error, Result, EBADF, EBADFD, EEXIST, ENOENT, EPERM, MODE_CHR, O_CREAT, O_EXCL, O_RDONLY,
    O_RDWR, O_WRONLY,
};

// Create an RNG Seed to create initial seed from the rdrand intel instruction
use rand_core::SeedableRng;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::num::Wrapping;

// This Daemon implements a Cryptographically Secure Random Number Generator
// that does not block on read - i.e. it is equivalent to linux /dev/urandom
// We do not implement blocking reads as per linux /dev/random for the reasons outlined
// here: https://www.2uo.de/myths-about-urandom/

// Default file access mode for PRNG
const DEFAULT_PRNG_MODE: u16 = 0o644;
// Rand crate recommends at least 256 bits of entropy to seed the RNG
const SEED_BYTES: usize = 32;

/// Create a true random seed for the RNG from the Intel x64 rdrand instruction if present.
/// Will seed with a zero (insecure) if rdrand not present.
fn create_rdrand_seed() -> [u8; SEED_BYTES] {
    let mut rng = [0; SEED_BYTES];
    let mut have_seeded = false;
    #[cfg(target_arch = "x86_64")]
    {
        if CpuId::new().get_feature_info().unwrap().has_rdrand() {
            for i in 0..SEED_BYTES / 8 {
                // We get 8 bytes at a time from rdrand instruction
                let rand: u64;
                unsafe {
                    asm!("rdrand rax", out("rax") rand);
                }

                rng[i * 8..(i * 8 + 8)].copy_from_slice(&rand.to_le_bytes());
            }
            have_seeded = true;
        }
    } // TODO integrate alternative entropy sources
    if !have_seeded {
        println!("randd: Seeding failed, no entropy source.  Random numbers on this platform are NOT SECURE");
    }
    rng
}

/// Contains information about an open file
struct OpenFileInfo {
    o_flags: usize,
    /// Flags used when opening file.
    uid: u32,
    gid: u32,
    file_stat: Stat,
}

impl OpenFileInfo {
    /// Tests if the current user has enough permissions to view the file, op is the operation,
    /// like read and write, these modes are MODE_EXEC, MODE_READ, and MODE_WRITE
    /// Copied from redoxfs
    fn permission(&self, op: u16) -> bool {
        let mut perm = self.file_stat.st_mode & 0o7;
        if self.uid == self.file_stat.st_uid {
            // If self.mode is 101100110, >> 6 would be 000000101
            // 0o7 is octal for 111, or, when expanded to 9 digits is 000000111
            perm |= (self.file_stat.st_mode >> 6) & 0o7;
            // Since we erased the GID and OTHER bits when >>6'ing, |= will keep those bits in place.
        }
        if self.gid == self.file_stat.st_gid || self.file_stat.st_gid == 0 {
            perm |= (self.file_stat.st_mode >> 3) & 0o7;
        }
        if self.uid == 0 {
            //set the `other` bits to 111
            perm |= 0o7;
        }
        perm & op == op
    }
    fn o_flag_set(&self, f: usize) -> bool {
        return (f & self.o_flags) == f;
    }
}

/// Struct to represent the rand scheme.
struct RandScheme {
    prng: ChaCha20Rng,
    // ChaCha20 is a Cryptographically Secure PRNG
    // https://docs.rs/rand/0.5.0/rand/prng/chacha/struct.ChaChaRng.html
    // Allows 2^64 streams of random numbers, which we will equate with file numbers
    prng_stat: Stat,
    open_descriptors: BTreeMap<usize, OpenFileInfo>, // Cannot use HashMap as the implementation
    // calls the system RNG (us) for entropy to protect against HashDOS attacks.
    // Trying to create a HashMap causes a system crash.
    // <file number, information about the open file>
    next_fd: Wrapping<usize>,
}

impl RandScheme {
    /// Create new rand scheme from a message socket
    fn new() -> RandScheme {
        RandScheme {
            prng: ChaCha20Rng::from_seed(create_rdrand_seed()),
            prng_stat: Stat {
                st_mode: MODE_CHR | DEFAULT_PRNG_MODE,
                st_gid: 0,
                st_uid: 0,
                ..Default::default()
            },
            open_descriptors: BTreeMap::new(),
            next_fd: Wrapping(0),
        }
    }

    /// Gets the open file info for a file descriptor if it is open - error otherwise.
    fn get_fd(&self, fd: usize) -> Result<&OpenFileInfo> {
        // Check we've got a valid file descriptor
        let file_info = match self.open_descriptors.get(&fd) {
            Some(m) => m,
            None => return Err(Error::new(EBADF)),
        };
        Ok(file_info)
    }
    /// Checks to see if the op (MODE_READ, MODE_WRITE) can be performed on the open file
    /// descriptor - Will return the open file info if successful, and error if the file
    /// descriptor is invalid, or the permission is denied.
    fn can_perform_op_on_fd(&self, fd: usize, op: u16) -> Result<&OpenFileInfo> {
        let file_info = self.get_fd(fd)?;
        if !file_info.permission(op) {
            return Err(Error::new(EPERM));
        }
        Ok(file_info)
    }
    /// Reseed the CSPRNG with the supplied entropy.
    /// TODO add this to an entropy pool and give a limited estimate to the amount of entropy
    /// TODO consider having trusted and untrusted entropy URIs, with different permissions.
    fn reseed_prng(&mut self, entropy: &[u8]) {
        // Need to fill a fixed size array for the from_seed, so we'll do 256 bit
        // array and has the entropy into it.
        let mut digest = Sha256::new();
        digest.input(entropy);
        let hash = digest.result();
        let mut entropy_array: [u8; SEED_BYTES] = [0; SEED_BYTES];
        entropy_array.copy_from_slice(hash.as_slice());
        self.prng = ChaCha20Rng::from_seed(entropy_array);
    }
}

#[test]
fn test_scheme_perms() {
    use syscall::{O_CLOEXEC, O_STAT};

    let mut scheme = RandScheme::new();
    scheme.prng_stat.st_mode = MODE_CHR | 0o200;
    scheme.prng_stat.st_uid = 1;
    scheme.prng_stat.st_gid = 1;
    assert!(scheme.open("/", O_RDWR, 1, 1).is_err());
    assert!(scheme.open("/", O_RDONLY, 1, 1).is_err());

    scheme.prng_stat.st_mode = MODE_CHR | 0o400;
    let mut fd = scheme.open("", O_RDONLY, 1, 1).unwrap();
    assert!(scheme.can_perform_op_on_fd(fd, MODE_READ).is_ok());
    assert!(scheme.can_perform_op_on_fd(fd, MODE_WRITE).is_err());
    assert!(scheme.close(fd).is_ok());

    assert!(scheme.open("", O_WRONLY, 1, 1).is_err());
    assert!(scheme.open("", O_RDWR, 1, 1).is_err());

    scheme.prng_stat.st_mode = MODE_CHR | 0o600;
    fd = scheme.open("", O_RDWR, 1, 1).unwrap();
    assert!(scheme.can_perform_op_on_fd(fd, MODE_READ).is_ok());
    assert!(scheme.can_perform_op_on_fd(fd, MODE_WRITE).is_ok());
    assert!(scheme.close(fd).is_ok());

    fd = scheme.open("", O_STAT, 2, 2).unwrap();
    assert!(scheme.can_perform_op_on_fd(fd, MODE_READ).is_err());
    assert!(scheme.can_perform_op_on_fd(fd, MODE_WRITE).is_err());
    assert!(scheme.close(fd).is_ok());
    fd = scheme.open("", O_STAT | O_CLOEXEC, 2, 2).unwrap();
    assert!(scheme.can_perform_op_on_fd(fd, MODE_READ).is_err());
    assert!(scheme.can_perform_op_on_fd(fd, MODE_WRITE).is_err());
    assert!(scheme.close(fd).is_ok());

    // Try another user in group (no group perms)
    fd = scheme.open("", O_STAT | O_CLOEXEC, 2, 1).unwrap();
    assert!(scheme.can_perform_op_on_fd(fd, MODE_READ).is_err());
    assert!(scheme.can_perform_op_on_fd(fd, MODE_WRITE).is_err());
    assert!(scheme.close(fd).is_ok());
    scheme.prng_stat.st_mode = MODE_CHR | 0o660;
    fd = scheme.open("", O_STAT | O_CLOEXEC, 2, 1).unwrap();
    assert!(scheme.can_perform_op_on_fd(fd, MODE_READ).is_ok());
    assert!(scheme.can_perform_op_on_fd(fd, MODE_WRITE).is_ok());
    assert!(scheme.close(fd).is_ok());

    // Check root can do anything
    scheme.prng_stat.st_mode = MODE_CHR | 0o000;
    fd = scheme.open("", O_STAT | O_CLOEXEC, 0, 0).unwrap();
    assert!(scheme.can_perform_op_on_fd(fd, MODE_READ).is_ok());
    assert!(scheme.can_perform_op_on_fd(fd, MODE_WRITE).is_ok());
    assert!(scheme.close(fd).is_ok());

    // Check the rand:/urandom URL (Equivalent to rand:/)
    scheme.prng_stat.st_mode = MODE_CHR | 0o660;
    fd = scheme.open("/urandom", O_STAT | O_CLOEXEC, 2, 1).unwrap();
    assert!(scheme.can_perform_op_on_fd(fd, MODE_READ).is_ok());
    assert!(scheme.can_perform_op_on_fd(fd, MODE_WRITE).is_ok());
    assert!(scheme.close(fd).is_ok());
}

impl SchemeMut for RandScheme {
    fn open(&mut self, path: &str, flags: usize, uid: u32, gid: u32) -> Result<usize> {
        // We are only allowing
        // reads/writes from rand:/ and rand:/urandom - the root directory on its own is passed as an empty slice
        if path != "" && path != "/urandom" {
            return Err(Error::new(ENOENT));
        }
        if flags & (O_CREAT | O_EXCL) == O_CREAT | O_EXCL {
            return Err(Error::new(EEXIST));
        }

        let fd = self.next_fd;
        let open_file_info = OpenFileInfo {
            o_flags: flags,
            file_stat: self.prng_stat,
            uid,
            gid,
        };

        if (open_file_info.o_flag_set(O_RDONLY) || open_file_info.o_flag_set(O_RDWR))
            && !open_file_info.permission(MODE_READ)
        {
            return Err(Error::new(EPERM));
        }
        if (open_file_info.o_flag_set(O_WRONLY) || open_file_info.o_flag_set(O_RDWR))
            && !open_file_info.permission(MODE_WRITE)
        {
            return Err(Error::new(EPERM));
        }
        self.open_descriptors.insert(fd.0, open_file_info);
        // Get the next file descriptor
        self.next_fd += Wrapping(1);
        // If we've looped round there's a small chance that the file descriptor still exists, so loop till we get one that doesn't
        loop {
            if !self.open_descriptors.contains_key(&self.next_fd.0) {
                break;
            } else {
                self.next_fd += Wrapping(1);
            }
        }
        Ok(fd.0)
    }

    /* Resource operations */
    fn read(&mut self, file: usize, buf: &mut [u8], _offset: u64, _flags: u32) -> Result<usize> {
        // Check fd and permissions
        self.can_perform_op_on_fd(file, MODE_READ)?;

        // Setting the stream will ensure that if two clients are reading concurrently, they won't get the same numbers
        self.prng.set_stream(file as u64); // Should probably find a way to re-instate the counter for this stream, but
                                           // not doing so won't make the output any less 'random'
        self.prng.fill_bytes(buf);

        Ok(buf.len())
    }

    fn write(&mut self, file: usize, buf: &[u8], _offset: u64, _flags: u32) -> Result<usize> {
        // Check fd and permissions
        self.can_perform_op_on_fd(file, MODE_WRITE)?;

        // TODO - when we support other entropy sources, just add this to an entropy pool
        // TODO - consider having trusted and untrusted entropy writing paths
        // We have a healthy mistrust of the entropy we're being given, so we won't seed just with
        // that as the resulting numbers would be predictable based on this input
        // we'll take 512 bits (arbitrary) from the current PRNG, and seed with that
        // and the supplied data.

        let mut rng_buf: [u8; SEED_BYTES] = [0; SEED_BYTES];
        self.prng.fill_bytes(&mut rng_buf);
        let mut rng_vec = Vec::new();
        rng_vec.extend(&rng_buf);
        rng_vec.extend(buf);
        self.reseed_prng(&rng_vec);
        Ok(buf.len())
    }

    fn fchmod(&mut self, file: usize, mode: u16) -> Result<usize> {
        // Check fd and permissions
        let file_info = self.get_fd(file)?;
        // only root and owner can chmod
        if file_info.uid != file_info.file_stat.st_uid && file_info.uid != 0 {
            return Err(Error::new(EPERM));
        }

        self.prng_stat.st_mode = MODE_CHR | mode;
        Ok(0)
    }

    fn fchown(&mut self, file: usize, uid: u32, gid: u32) -> Result<usize> {
        // Check fd and permissions
        let file_info = self.get_fd(file)?;
        // only root and owner can chmod
        if file_info.uid != file_info.file_stat.st_uid && file_info.uid != 0 {
            return Err(Error::new(EPERM));
        }

        self.prng_stat.st_uid = uid;
        self.prng_stat.st_gid = gid;
        Ok(0)
    }

    fn fcntl(&mut self, _id: usize, _cmd: usize, _arg: usize) -> Result<usize> {
        // Just ignore this.
        Ok(0)
    }

    fn fevent(&mut self, _id: usize, _flags: EventFlags) -> Result<EventFlags> {
        Ok(EventFlags::EVENT_READ)
    }
    fn fpath(&mut self, _file: usize, buf: &mut [u8]) -> Result<usize> {
        let mut i = 0;
        let scheme_path = b"rand";
        while i < buf.len() && i < scheme_path.len() {
            buf[i] = scheme_path[i];
            i += 1;
        }
        Ok(i)
    }

    fn fstat(&mut self, file: usize, stat: &mut Stat) -> Result<usize> {
        // Check fd and permissions
        self.can_perform_op_on_fd(file, MODE_READ)?;

        *stat = self.prng_stat.clone();

        Ok(0)
    }

    fn close(&mut self, file: usize) -> Result<usize> {
        // just remove the file descriptor from the open descriptors
        match self.open_descriptors.remove(&file) {
            Some(_) => Ok(0),
            None => Err(Error::new(EBADFD)),
        }
    }
}

fn daemon(daemon: redox_daemon::Daemon) -> ! {
    let socket = match Socket::<V2>::create("rand") {
        Ok(s) => s,
        Err(err) => {
            // Tolerate EEXIST (scheme already registered by an earlier instance)
            if err.errno == EEXIST {
                eprintln!("randd: rand scheme already exists, continuing");
                // Open existing scheme socket instead of creating
                Socket::<V2>::nonblock("rand").expect("randd: failed to open existing rand scheme")
            } else {
                panic!("randd: failed to create rand scheme: {}", err);
            }
        }
    };

    let mut scheme = RandScheme::new();

    daemon
        .ready()
        .expect("randd: failed to mark daemon as ready");

    libredox::call::setrens(0, 0).expect("randd: failed to enter null namespace");

    while let Some(request) = socket
        .next_request(SignalBehavior::Restart)
        .expect("error reading packet")
    {
        let RequestKind::Call(call) = request.kind() else {
            continue;
        };
        let response = call.handle_scheme_mut(&mut scheme);
        socket
            .write_responses(&[response], SignalBehavior::Restart)
            .expect("error writing packet");
    }

    process::exit(0);
}

fn main() {
    redox_daemon::Daemon::new(daemon).unwrap_or_else(|e| {
        eprintln!("randd: failed to daemonize: {}", e);
        std::process::exit(1);
    });
}
