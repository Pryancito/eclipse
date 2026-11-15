//! The FAT volume, and what the kernel asks of it: a directory's entries,
//! a file's bytes, the volume's numbers.
//!
//! A request names its file by path. Opening a file by path walks its
//! directories, and FAT finds a place in a file by walking its chain of
//! clusters from the start, so the files read last are remembered here as
//! where on the disk their bytes are ([`Run`]): a read at any offset is
//! then a read of the disk at a known place, with no walk and none of the
//! FAT code in the way. They are only that -- a path asked for again after
//! its file was let go is opened again.

use std::cell::RefCell;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::rc::Rc;

use fatfs::{DefaultTimeProvider, FsOptions, LossyOemCpConverter, StdIoWrapper};

use crate::disk::{Blocks, Disk};
use crate::wire::{self, Entry, STATFS_LEN};

/// The files remembered.
const FILES_KEPT: usize = 32;

/// `statfs`'s magic for FAT: `MSDOS_SUPER_MAGIC`.
const MSDOS_SUPER_MAGIC: u64 = 0x4d44;

/// The disk, as the FAT code has it and as the reads of file data have it.
struct Shared<B>(Rc<RefCell<Disk<B>>>);

impl<B: Blocks> Read for Shared<B> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.0.borrow_mut().read(buf)
    }
}

impl<B: Blocks> Seek for Shared<B> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.0.borrow_mut().seek(pos)
    }
}

impl<B: Blocks> Write for Shared<B> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.borrow_mut().write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

type Fs<B> = fatfs::FileSystem<StdIoWrapper<Shared<B>>, DefaultTimeProvider, LossyOemCpConverter>;

/// A Linux `errno`, as the kernel takes it.
pub type Errno = i32;

fn errno<E>(e: fatfs::Error<E>) -> Errno {
    match e {
        fatfs::Error::NotFound => libc::ENOENT,
        fatfs::Error::AlreadyExists => libc::EEXIST,
        fatfs::Error::DirectoryIsNotEmpty => libc::ENOTEMPTY,
        fatfs::Error::NotEnoughSpace => libc::ENOSPC,
        fatfs::Error::InvalidInput | fatfs::Error::InvalidFileNameLength => libc::EINVAL,
        _ => libc::EIO,
    }
}

/// Days from 1970-01-01 to the civil date, which may be before it.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// A date of a directory entry as seconds since 1970, the entry's clock
/// read as UTC, which is how the kernel's own FAT reads it.
fn unix_from_date(date: fatfs::Date) -> i64 {
    days_from_civil(date.year as i64, date.month as i64, date.day as i64) * 86_400
}

fn unix_from_date_time(at: fatfs::DateTime) -> i64 {
    unix_from_date(at.date)
        + at.time.hour as i64 * 3600
        + at.time.min as i64 * 60
        + at.time.sec as i64
}

/// A stretch of a file that is one stretch of the disk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Run {
    /// Where in the file it starts, and on the disk.
    file: u64,
    disk: u64,
    len: u64,
}

/// A file read lately: its path and where its bytes are.
struct Open {
    path: String,
    runs: Vec<Run>,
    size: u64,
}

/// The clusters of a file, each `(place on the disk, bytes)`, as runs:
/// clusters that follow one another on the disk are one run.
fn runs_of(clusters: impl Iterator<Item = (u64, u64)>) -> (Vec<Run>, u64) {
    let mut runs: Vec<Run> = Vec::new();
    let mut size = 0;
    for (disk, len) in clusters {
        match runs.last_mut() {
            Some(last) if last.disk + last.len == disk => last.len += len,
            _ => runs.push(Run {
                file: size,
                disk,
                len,
            }),
        }
        size += len;
    }
    (runs, size)
}

pub struct Volume<B: Blocks> {
    fs: Fs<B>,
    disk: Rc<RefCell<Disk<B>>>,
    /// The files read last, the most recent at the end.
    open: Vec<Open>,
    /// The directory listed last: one that does not fit a reply is asked
    /// for again from where the reply stopped.
    listed: Option<(String, Vec<Entry>)>,
    /// How many times a file was opened by path.
    pub opens: u64,
}

impl<B: Blocks> Volume<B> {
    /// The FAT volume on `blocks`, which holds `len` bytes: an error when
    /// it is not one.
    pub fn open(blocks: B, len: u64) -> Result<Self, String> {
        let disk = Rc::new(RefCell::new(Disk::new(blocks, len)));
        let fs = Fs::new(Shared(disk.clone()), FsOptions::new()).map_err(|e| format!("{e:?}"))?;
        Ok(Volume {
            fs,
            disk,
            open: Vec::new(),
            listed: None,
            opens: 0,
        })
    }

    /// How many times the device was read.
    pub fn device_reads(&self) -> u64 {
        self.disk.borrow().device_reads
    }

    fn entries(&self, path: &str) -> Result<Vec<Entry>, Errno> {
        let root = self.fs.root_dir();
        let dir = match path {
            "" => root,
            path => root.open_dir(path).map_err(errno)?,
        };
        let mut entries = Vec::new();
        for entry in dir.iter() {
            let entry = entry.map_err(errno)?;
            let name = entry.file_name();
            // Names the kernel would refuse the whole listing for.
            if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\0']) {
                continue;
            }
            entries.push(Entry {
                name,
                is_dir: entry.is_dir(),
                size: entry.len(),
                mtime: unix_from_date_time(entry.modified()),
                atime: unix_from_date(entry.accessed()),
                ctime: unix_from_date_time(entry.created()),
            });
        }
        Ok(entries)
    }

    /// `OP_LIST`: the entries of the directory `path` from entry `start`
    /// on, as records in `out`: `(records, bytes, next)`.
    pub fn list(
        &mut self,
        path: &str,
        start: u64,
        out: &mut [u8],
    ) -> Result<(usize, usize, u64), Errno> {
        // From the top the directory is read again: it is a new listing.
        let kept = start > 0 && self.listed.as_ref().is_some_and(|(p, _)| p == path);
        if !kept {
            self.listed = Some((path.to_string(), self.entries(path)?));
        }
        let (_, entries) = self.listed.as_ref().expect("just listed");
        let packed = wire::pack(entries, start, out);
        if packed.2 == wire::LIST_END {
            self.listed = None;
        }
        Ok(packed)
    }

    /// `OP_READ`: the bytes of the file `path` from `offset`, as many as
    /// `out` holds or the file has.
    pub fn read(&mut self, path: &str, offset: u64, out: &mut [u8]) -> Result<usize, Errno> {
        let open = match self.open.iter().position(|o| o.path == path) {
            Some(i) => self.open.remove(i),
            None => {
                self.opens += 1;
                let mut file = self.fs.root_dir().open_file(path).map_err(errno)?;
                let mut clusters = Vec::new();
                for extent in file.extents() {
                    let extent = extent.map_err(errno)?;
                    clusters.push((extent.offset, extent.size as u64));
                }
                let (runs, size) = runs_of(clusters.into_iter());
                if self.open.len() >= FILES_KEPT {
                    self.open.remove(0);
                }
                Open {
                    path: path.to_string(),
                    runs,
                    size,
                }
            }
        };
        let read = self.read_runs(&open, offset, out);
        self.open.push(open);
        read
    }

    fn read_runs(&self, open: &Open, offset: u64, out: &mut [u8]) -> Result<usize, Errno> {
        let mut disk = self.disk.borrow_mut();
        let mut done = 0;
        while done < out.len() {
            let at = offset + done as u64;
            if at >= open.size {
                break;
            }
            // The last run that starts at or before `at`: there is one,
            // the first starting at 0, and `at` is inside the file.
            let run = open.runs[open.runs.partition_point(|r| r.file <= at) - 1];
            let within = at - run.file;
            let n = ((run.len - within) as usize).min(out.len() - done);
            disk.read_exact_at(run.disk + within, &mut out[done..done + n])
                .map_err(|_| libc::EIO)?;
            done += n;
        }
        Ok(done)
    }

    /// `OP_STATFS`: the cluster size, the clusters, the free ones, the
    /// longest name and the magic, into `out`. The bytes written.
    pub fn statfs(&self, out: &mut [u8]) -> Result<usize, Errno> {
        let stats = self.fs.stats().map_err(errno)?;
        let numbers = [
            stats.cluster_size() as u64,
            stats.total_clusters() as u64,
            stats.free_clusters() as u64,
            255,
            MSDOS_SUPER_MAGIC,
        ];
        for (i, n) in numbers.iter().enumerate() {
            out[i * 8..i * 8 + 8].copy_from_slice(&n.to_ne_bytes());
        }
        Ok(STATFS_LEN)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    const BIG: usize = 300_000;

    fn big() -> Vec<u8> {
        (0..BIG).map(|i| (i * 7 + i / 251) as u8).collect()
    }

    /// A volume of 8 MiB with a few files and directories on it, written
    /// with the same FAT code and then read back through [`Disk`].
    fn volume() -> Volume<Vec<u8>> {
        let mut image = Cursor::new(vec![0u8; 8 << 20]);
        fatfs::format_volume(
            &mut StdIoWrapper::new(&mut image),
            fatfs::FormatVolumeOptions::new(),
        )
        .unwrap();
        {
            let fs = fatfs::FileSystem::new(&mut image, FsOptions::new()).unwrap();
            let root = fs.root_dir();
            root.create_file("hello.txt")
                .unwrap()
                .write_all(b"hello, world\n")
                .unwrap();
            let docs = root.create_dir("docs").unwrap();
            docs.create_file("A long name with spaces.md")
                .unwrap()
                .write_all(b"# title\n")
                .unwrap();
            docs.create_dir("deep")
                .unwrap()
                .create_file("big.bin")
                .unwrap()
                .write_all(&big())
                .unwrap();
            let many = root.create_dir("many").unwrap();
            for i in 0..300 {
                many.create_file(&format!("file-number-{i:04}.dat"))
                    .unwrap();
            }
        }
        let image = image.into_inner();
        let len = image.len() as u64;
        Volume::open(image, len).unwrap()
    }

    fn names(vol: &mut Volume<Vec<u8>>, path: &str) -> Vec<(String, bool, u64)> {
        let entries = vol.entries(path).unwrap();
        entries
            .into_iter()
            .map(|e| (e.name, e.is_dir, e.size))
            .collect()
    }

    #[test]
    fn a_directory_lists_its_files_and_directories() {
        let mut vol = volume();
        assert_eq!(
            names(&mut vol, ""),
            [
                ("hello.txt".to_string(), false, 13),
                ("docs".to_string(), true, 0),
                ("many".to_string(), true, 0),
            ]
        );
        // A directory further down has no `.` or `..` among its entries.
        assert_eq!(
            names(&mut vol, "docs"),
            [
                ("A long name with spaces.md".to_string(), false, 8),
                ("deep".to_string(), true, 0),
            ]
        );
        assert_eq!(vol.entries("nowhere").unwrap_err(), libc::ENOENT);
    }

    #[test]
    fn a_listing_that_does_not_fit_one_reply_is_whole_over_several() {
        let mut vol = volume();
        let mut out = vec![0u8; 4096];
        let (mut start, mut total, mut replies) = (0, 0, 0);
        while start != wire::LIST_END {
            let (count, used, next) = vol.list("many", start, &mut out).unwrap();
            assert!(count > 0 && used <= out.len());
            total += count;
            replies += 1;
            start = next;
        }
        assert_eq!(total, 300);
        assert!(replies > 1);
    }

    #[test]
    fn a_file_reads_the_bytes_it_holds_from_any_offset() {
        let mut vol = volume();
        let mut out = vec![0u8; 64];
        assert_eq!(vol.read("hello.txt", 0, &mut out), Ok(13));
        assert_eq!(&out[..13], b"hello, world\n");
        assert_eq!(vol.read("hello.txt", 7, &mut out), Ok(6));
        assert_eq!(&out[..6], b"world\n");
        // At the end and past it there is nothing.
        assert_eq!(vol.read("hello.txt", 13, &mut out), Ok(0));
        assert_eq!(vol.read("hello.txt", 5000, &mut out), Ok(0));
        assert_eq!(vol.read("no-such-file", 0, &mut out), Err(libc::ENOENT));

        let big = big();
        let mut out = vec![0u8; 70_000];
        for at in [0usize, 65_536, 131_072, 12_345, 299_990] {
            let n = vol.read("docs/deep/big.bin", at as u64, &mut out).unwrap();
            assert_eq!(n, out.len().min(BIG - at), "at {at}");
            assert_eq!(out[..n], big[at..at + n], "at {at}");
        }
    }

    #[test]
    fn a_file_is_opened_once_however_it_is_read() {
        let mut vol = volume();
        let mut out = vec![0u8; 65_536];
        let mut at = 0;
        loop {
            let n = vol.read("docs/deep/big.bin", at, &mut out).unwrap();
            if n == 0 {
                break;
            }
            at += n as u64;
        }
        assert_eq!(at as usize, BIG);
        // Another file between two reads, and reads that go back.
        vol.read("hello.txt", 0, &mut out).unwrap();
        for at in [200_000, 0, 123_456] {
            vol.read("docs/deep/big.bin", at, &mut out).unwrap();
        }
        assert_eq!(vol.opens, 2);
    }

    #[test]
    fn a_read_at_any_offset_costs_the_device_what_it_reads() {
        let mut vol = volume();
        let mut out = vec![0u8; 65_536];
        vol.read("docs/deep/big.bin", 0, &mut out).unwrap();
        // The file was written in one go on an empty volume: one run, so a
        // window of it anywhere is one read of the device, and the chain
        // of clusters is not walked to get there.
        for at in [262_144u64, 65_536, 196_608] {
            let before = vol.device_reads();
            assert_eq!(
                vol.read("docs/deep/big.bin", at, &mut out),
                Ok(BIG.min(at as usize + 65_536) - at as usize)
            );
            assert!(vol.device_reads() - before <= 2, "at {at}");
        }
    }

    #[test]
    fn clusters_that_follow_one_another_are_one_run() {
        let clusters = [(4096, 512), (4608, 512), (9000, 512), (9512, 100)];
        let (runs, size) = runs_of(clusters.into_iter());
        assert_eq!(size, 1636);
        assert_eq!(
            runs,
            [
                Run {
                    file: 0,
                    disk: 4096,
                    len: 1024
                },
                Run {
                    file: 1024,
                    disk: 9000,
                    len: 612
                },
            ]
        );
        assert_eq!(runs_of(std::iter::empty()), (vec![], 0));
    }

    #[test]
    fn more_files_than_are_kept_open_still_read() {
        let mut vol = volume();
        let mut out = vec![0u8; 16];
        for round in 0..2 {
            for i in 0..FILES_KEPT + 4 {
                let path = format!("many/file-number-{i:04}.dat");
                assert_eq!(vol.read(&path, 0, &mut out), Ok(0), "round {round}");
            }
        }
        assert_eq!(vol.read("hello.txt", 0, &mut out), Ok(13));
    }

    #[test]
    fn the_numbers_are_the_volumes() {
        let vol = volume();
        let mut out = [0u8; STATFS_LEN];
        assert_eq!(vol.statfs(&mut out), Ok(STATFS_LEN));
        let field = |i: usize| u64::from_ne_bytes(out[i * 8..i * 8 + 8].try_into().unwrap());
        let (cluster, total, free) = (field(0), field(1), field(2));
        assert!(cluster >= 512 && cluster.is_power_of_two());
        assert!(total * cluster <= 8 << 20 && total * cluster > 7 << 20);
        assert!(free < total && free > 0);
        assert_eq!(field(4), 0x4d44);
    }

    #[test]
    fn a_date_is_seconds_since_1970() {
        let date = fatfs::Date::new(2024, 2, 29);
        assert_eq!(unix_from_date(date), 1_709_164_800);
        let at = fatfs::DateTime::new(date, fatfs::Time::new(13, 14, 15, 0));
        assert_eq!(
            unix_from_date_time(at),
            1_709_164_800 + 13 * 3600 + 14 * 60 + 15
        );
    }

    #[test]
    fn what_is_not_a_fat_volume_is_not_opened() {
        let image = vec![0x55u8; 1 << 20];
        assert!(Volume::open(image, 1 << 20).is_err());
    }
}
