use crate::serial;
pub struct Filesystem {}
impl Filesystem {
    pub const PARTITION_OFFSET_BLOCKS: u64 = 131328;
    pub fn mount() -> Result<(), &'static str> { Err("Disabled") }
    pub fn read_file_by_inode(_inode: u32, _buf: &mut [u8]) -> Result<usize, &'static str> { Err("Disabled") }
    pub fn write_file_by_inode(_inode: u32, _buf: &[u8], _offset: u64) -> Result<usize, &'static str> { Err("Disabled") }
    pub fn lookup_path(_path: &str) -> Result<u32, &'static str> { Err("Disabled") }
    pub fn get_file_size(_inode: u32) -> Result<u64, &'static str> { Err("Disabled") }
}
pub fn init() {}
pub fn mount_root() -> Result<(), &'static str> { Err("Disabled") }
pub fn is_mounted() -> bool { false }
