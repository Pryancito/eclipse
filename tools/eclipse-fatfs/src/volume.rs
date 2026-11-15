//! The FAT volume, and what the kernel asks of it: a directory's entries,
//! a file's bytes read or written, a file or a directory made, removed or
//! renamed, the volume's numbers.
//!
//! A request names its file by path. Opening a file by path walks its
//! directories, and FAT finds a place in a file by walking its chain of
//! clusters from the start, so the files used last are remembered here as
//! where on the disk their bytes are ([`Run`]): a read at any offset, or a
//! write over bytes the file already has, is then a read or a write of the
//! disk at a known place, with no walk and none of the FAT code in the
//! way. They are only that -- a path asked for again after its file was let
//! go is opened again.
//!
//! A write that makes the file longer is the FAT code's: it finds the
//! clusters. The file written to last stays open ([`Appending`]) at where
//! the write ended, so that a file written in order is not walked to its
//! end for each piece.

use std::cell::RefCell;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::rc::Rc;
use std::time::{SystemTime, UNIX_EPOCH};

use fatfs::{FsOptions, LossyOemCpConverter, StdIoWrapper};

use crate::disk::{Blocks, Disk, Kept};
use crate::wire::{self, Entry, STATFS_LEN};

/// The files remembered.
const FILES_KEPT: usize = 32;

/// `statfs`'s magic for FAT: `MSDOS_SUPER_MAGIC`.
const MSDOS_SUPER_MAGIC: u64 = 0x4d44;

/// The longest file FAT has: its size is 32 bits.
const MAX_FILE: u64 = u32::MAX as u64;

/// The disk, as the FAT code has it and as the reads and writes of file
/// data have it.
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

/// The clock the entries written are stamped with: this process's, read as
/// UTC, which is how the entries are read back.
#[derive(Clone, Copy, Debug)]
pub struct Clock;

impl fatfs::TimeProvider for Clock {
    fn get_current_date(&self) -> fatfs::Date {
        self.get_current_date_time().date
    }

    fn get_current_date_time(&self) -> fatfs::DateTime {
        date_time_from_unix(unix_now())
    }
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

type Io<B> = StdIoWrapper<Shared<B>>;
type Fs<B> = fatfs::FileSystem<Io<B>, Clock, LossyOemCpConverter>;
type FatError = fatfs::Error<io::Error>;

/// A Linux `errno`, as the kernel takes it.
pub type Errno = i32;

fn errno(e: FatError) -> Errno {
    match e {
        fatfs::Error::NotFound => libc::ENOENT,
        fatfs::Error::AlreadyExists => libc::EEXIST,
        fatfs::Error::DirectoryIsNotEmpty => libc::ENOTEMPTY,
        fatfs::Error::NotEnoughSpace => libc::ENOSPC,
        fatfs::Error::InvalidInput
        | fatfs::Error::InvalidFileNameLength
        | fatfs::Error::UnsupportedFileNameCharacter => libc::EINVAL,
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

/// The civil date `days` after 1970-01-01: year, month, day.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
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

/// Seconds since 1970 as a directory entry's date and time, which FAT has
/// from 1980 to 2107: a time outside is the nearest it has.
fn date_time_from_unix(secs: i64) -> fatfs::DateTime {
    let first = days_from_civil(1980, 1, 1) * 86_400;
    let last = days_from_civil(2108, 1, 1) * 86_400 - 1;
    let secs = secs.clamp(first, last);
    let (y, m, d) = civil_from_days(secs.div_euclid(86_400));
    let of_day = secs.rem_euclid(86_400);
    fatfs::DateTime::new(
        fatfs::Date::new(y as u16, m as u16, d as u16),
        fatfs::Time::new(
            (of_day / 3600) as u16,
            (of_day / 60 % 60) as u16,
            (of_day % 60) as u16,
            0,
        ),
    )
}

/// A stretch of a file that is one stretch of the disk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Run {
    /// Where in the file it starts, and on the disk.
    file: u64,
    disk: u64,
    len: u64,
}

/// A file used lately: its path and where its bytes are.
struct Open {
    path: String,
    runs: Vec<Run>,
    size: u64,
    /// The second its entry was last stamped for a write over its bytes:
    /// once a second is enough.
    stamped: i64,
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

/// [`Disk::word`]: it was set, and the volume was clean when it was.
const NOTED: u64 = 1;
const WAS_CLEAN: u64 = 2;

/// The file being made longer, open at `pos`.
struct Appending<B: Blocks + 'static> {
    path: String,
    /// It borrows the volume's `fs`: see [`Volume::fs_for_appending`].
    file: fatfs::File<'static, Io<B>, Clock, LossyOemCpConverter>,
    pos: u64,
    size: u64,
}

pub struct Volume<B: Blocks + 'static> {
    /// Before `fs`, which it borrows: what is dropped first.
    appending: Option<Appending<B>>,
    /// `None` only when the volume could not be opened again after a
    /// [`sync`](Self::sync): everything is an I/O error from then on.
    fs: Option<Box<Fs<B>>>,
    disk: Rc<RefCell<Disk<B>>>,
    /// The files used last, the most recent at the end.
    open: Vec<Open>,
    /// The directory listed last: one that does not fit a reply is asked
    /// for again from where the reply stopped.
    listed: Option<(String, Vec<Entry>)>,
    /// Something was written since the volume was last left clean.
    changed: bool,
    /// This server took the volume over from one that died, and the
    /// volume was clean before the first of them mounted it: the mark that
    /// it is not, which the dead server made, is this one's to take off.
    /// The FAT code leaves a mark it finds, as it must when it cannot know
    /// whose it is.
    marks_clean: bool,
    /// How many times a file was opened by path.
    pub opens: u64,
}

/// A path's directory and its name in it.
fn split_path(path: &str) -> (&str, &str) {
    match path.rfind('/') {
        Some(i) => (&path[..i], &path[i + 1..]),
        None => ("", path),
    }
}

impl<B: Blocks + 'static> Volume<B> {
    /// The FAT volume on `blocks`, which holds `len` bytes: an error when
    /// it is not one.
    pub fn open(blocks: B, len: u64) -> Result<Self, String> {
        Self::open_over(blocks, len, Kept::private())
    }

    /// [`open`](Self::open) with what is written and has not reached the
    /// device kept in `kept`: new memory, or what the server before this
    /// one left, which is then part of the volume as much as the device is.
    pub fn open_over(blocks: B, len: u64, kept: Kept) -> Result<Self, String> {
        let disk = Disk::over(blocks, len, kept).ok_or("what was kept is another device's")?;
        let disk = Rc::new(RefCell::new(disk));
        let fs = Self::mount(&disk).map_err(|e| format!("{e:?}"))?;
        // Whether the volume was clean when the first server mounted it
        // is kept with the chunks, for the servers after it.
        let adopted = disk.borrow().adopted();
        if !adopted {
            let clean = !fs
                .read_status_flags()
                .map_err(|e| format!("{e:?}"))?
                .dirty();
            let word = NOTED | if clean { WAS_CLEAN } else { 0 };
            disk.borrow_mut().set_word(word);
        }
        let marks_clean = adopted && disk.borrow_mut().word() & WAS_CLEAN != 0;
        Ok(Volume {
            appending: None,
            fs: Some(Box::new(fs)),
            disk,
            open: Vec::new(),
            listed: None,
            changed: false,
            marks_clean,
            opens: 0,
        })
    }

    fn mount(disk: &Rc<RefCell<Disk<B>>>) -> Result<Fs<B>, FatError> {
        // The FAT code takes a stream that is at its start.
        disk.borrow_mut().seek(SeekFrom::Start(0))?;
        Fs::new(Shared(disk.clone()), FsOptions::new().time_provider(Clock))
    }

    fn fs(&self) -> Result<&Fs<B>, Errno> {
        self.fs.as_deref().ok_or(libc::EIO)
    }

    /// The filesystem, for the file kept open in `appending`.
    fn fs_for_appending(&self) -> Result<&'static Fs<B>, Errno> {
        let fs: *const Fs<B> = self.fs()?;
        // SAFETY: the filesystem is in a box, which does not move, and it
        // is only taken out of `self.fs` in `sync` and when the volume is
        // dropped, both after `appending` -- the only holder of what this
        // returns -- is: `sync` empties it first, and it is the field
        // declared, so dropped, before `fs`.
        Ok(unsafe { &*fs })
    }

    /// How many times the device was read.
    pub fn device_reads(&self) -> u64 {
        self.disk.borrow().device_reads
    }

    /// Whether something written has not reached the device.
    pub fn is_dirty(&self) -> bool {
        self.disk.borrow().is_dirty()
    }

    /// What was written reaches the device, as it is.
    pub fn flush(&mut self) -> Result<(), Errno> {
        self.disk.borrow_mut().flush().map_err(|_| libc::EIO)
    }

    /// Before a change that is not a write to the file being made longer:
    /// nothing is left open, and nothing remembered of `path`.
    fn changing(&mut self, path: &str) {
        self.appending = None;
        self.listed = None;
        self.changed = true;
        self.open.retain(|o| o.path != path);
    }

    /// The entries of the directory `path` that `keep` takes, up to the
    /// first it says is the last wanted.
    fn scan(
        &self,
        path: &str,
        mut keep: impl FnMut(&str) -> (bool, bool),
    ) -> Result<Vec<Entry>, Errno> {
        let root = self.fs()?.root_dir();
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
            let (wanted, last) = keep(&name);
            if wanted {
                entries.push(Entry {
                    name,
                    is_dir: entry.is_dir(),
                    size: entry.len(),
                    mtime: unix_from_date_time(entry.modified()),
                    atime: unix_from_date(entry.accessed()),
                    ctime: unix_from_date_time(entry.created()),
                });
            }
            if last {
                break;
            }
        }
        Ok(entries)
    }

    fn entries(&self, path: &str) -> Result<Vec<Entry>, Errno> {
        self.scan(path, |_| (true, false))
    }

    /// The entry at `path`, as its directory lists it.
    fn entry_at(&self, path: &str) -> Result<Entry, Errno> {
        let (dir, name) = split_path(path);
        let folded = name.to_lowercase();
        let is_it = |name: &str| {
            let found = name.to_lowercase() == folded;
            (found, found)
        };
        self.scan(dir, is_it)?.pop().ok_or(libc::ENOENT)
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

    /// Where the bytes of the file `path` are: it is the last of `open`
    /// when this returns.
    fn opened(&mut self, path: &str) -> Result<&mut Open, Errno> {
        let open = match self.open.iter().position(|o| o.path == path) {
            Some(i) => self.open.remove(i),
            None => {
                self.opens += 1;
                let (runs, size) = {
                    let mut file = self.fs()?.root_dir().open_file(path).map_err(errno)?;
                    let mut clusters = Vec::new();
                    for extent in file.extents() {
                        let extent = extent.map_err(errno)?;
                        clusters.push((extent.offset, extent.size as u64));
                    }
                    runs_of(clusters.into_iter())
                };
                if self.open.len() >= FILES_KEPT {
                    self.open.remove(0);
                }
                Open {
                    path: path.to_string(),
                    runs,
                    size,
                    stamped: 0,
                }
            }
        };
        self.open.push(open);
        Ok(self.open.last_mut().expect("just pushed"))
    }

    /// `OP_READ`: the bytes of the file `path` from `offset`, as many as
    /// `out` holds or the file has.
    pub fn read(&mut self, path: &str, offset: u64, out: &mut [u8]) -> Result<usize, Errno> {
        let disk = self.disk.clone();
        let open = self.opened(path)?;
        let len = (out.len() as u64).min(open.size.saturating_sub(offset)) as usize;
        Self::each_run(open, offset, len, |at, range| {
            disk.borrow_mut().read_exact_at(at, &mut out[range])
        })?;
        Ok(len)
    }

    /// `[offset, offset + len)` of the file, which has it, as the
    /// stretches of the disk it is: `(place on the disk, where in the
    /// range)` each.
    fn each_run(
        open: &Open,
        offset: u64,
        len: usize,
        mut each: impl FnMut(u64, std::ops::Range<usize>) -> io::Result<()>,
    ) -> Result<(), Errno> {
        let mut done = 0;
        while done < len {
            let at = offset + done as u64;
            // The last run that starts at or before `at`: there is one,
            // the first starting at 0, and `at` is inside the file.
            let run = open.runs[open.runs.partition_point(|r| r.file <= at) - 1];
            let within = at - run.file;
            let n = ((run.len - within) as usize).min(len - done);
            each(run.disk + within, done..done + n).map_err(|_| libc::EIO)?;
            done += n;
        }
        Ok(())
    }

    /// `OP_WRITE`: `data` into the file `path` from `offset`, the file
    /// zeros from its end up to there. The bytes written: fewer than given
    /// when the volume fills up.
    pub fn write(&mut self, path: &str, offset: u64, data: &[u8]) -> Result<usize, Errno> {
        let end = offset
            .checked_add(data.len() as u64)
            .filter(|end| *end <= MAX_FILE)
            .ok_or(libc::EFBIG)?;
        if data.is_empty() {
            return Ok(0);
        }
        self.listed = None;
        self.changed = true;
        let now = unix_now();
        let mut appending = match self.appending.take() {
            Some(appending) if appending.path == path => appending,
            _ => {
                // Over bytes the file has, they are where they are: no new
                // room, and none of the FAT code's work.
                let open = self.open.iter().position(|o| o.path == path);
                if open.is_some_and(|i| end <= self.open[i].size) {
                    return self.write_over(path, offset, data, now);
                }
                self.opens += 1;
                let fs = self.fs_for_appending()?;
                let mut file = fs.root_dir().open_file(path).map_err(errno)?;
                let size = fatfs::Seek::seek(&mut file, fatfs::SeekFrom::End(0)).map_err(errno)?;
                if open.is_none() && end <= size {
                    drop(file);
                    return self.write_over(path, offset, data, now);
                }
                Appending {
                    path: path.to_string(),
                    file,
                    pos: size,
                    size,
                }
            }
        };
        // Past its end: the file gets longer, and where its bytes are is
        // not what was remembered.
        self.open.retain(|o| o.path != path);
        let wrote = Self::extend(&mut appending, offset, data);
        // The entry has the new size, whatever became of the write.
        let flushed = fatfs::Write::flush(&mut appending.file).map_err(errno);
        self.appending = Some(appending);
        flushed.and(wrote)
    }

    /// Write `data` at `offset` of the file `path`, which has those bytes.
    fn write_over(
        &mut self,
        path: &str,
        offset: u64,
        data: &[u8],
        now: i64,
    ) -> Result<usize, Errno> {
        let disk = self.disk.clone();
        let open = self.opened(path)?;
        Self::each_run(open, offset, data.len(), |at, range| {
            disk.borrow_mut().write_all_at(at, &data[range])
        })?;
        if std::mem::replace(&mut open.stamped, now) != now {
            self.stamp(path, now)?;
        }
        Ok(data.len())
    }

    fn extend(appending: &mut Appending<B>, offset: u64, data: &[u8]) -> Result<usize, Errno> {
        let Appending {
            file, pos, size, ..
        } = appending;
        let seek = |file: &mut _, pos: &mut u64, to: u64| -> Result<(), Errno> {
            if *pos != to {
                *pos = fatfs::Seek::seek(file, fatfs::SeekFrom::Start(to)).map_err(errno)?;
            }
            Ok(())
        };
        // Write all of `bytes` at the position, or as much as there is
        // room for.
        let put = |file: &mut _, pos: &mut u64, size: &mut u64, bytes: &[u8]| {
            let mut done = 0;
            while done < bytes.len() {
                match fatfs::Write::write(file, &bytes[done..]) {
                    Ok(0) | Err(fatfs::Error::NotEnoughSpace) => break,
                    Ok(n) => {
                        done += n;
                        *pos += n as u64;
                        *size = (*size).max(*pos);
                    }
                    Err(e) => return Err(errno(e)),
                }
            }
            Ok(done)
        };
        if offset > *size {
            // FAT has no holes: the gap is written as zeros.
            seek(file, pos, *size)?;
            let zeros = [0u8; 4096];
            while *pos < offset {
                let n = ((offset - *pos) as usize).min(zeros.len());
                if put(file, pos, size, &zeros[..n])? < n {
                    return Err(libc::ENOSPC);
                }
            }
        }
        seek(file, pos, offset)?;
        match put(file, pos, size, data)? {
            0 => Err(libc::ENOSPC),
            n => Ok(n),
        }
    }

    /// The file `path` was last written at `secs`, in its entry.
    fn stamp(&mut self, path: &str, secs: i64) -> Result<(), Errno> {
        let mut file = self.fs()?.root_dir().open_file(path).map_err(errno)?;
        file.set_modified(date_time_from_unix(secs));
        fatfs::Write::flush(&mut file).map_err(errno)
    }

    /// `OP_SETTIME`: the file `path` was last written at `secs`. A
    /// directory's entry is not one the FAT code changes: nothing.
    pub fn set_time(&mut self, path: &str, secs: i64) -> Result<(), Errno> {
        if self.entry_at(path)?.is_dir {
            return Ok(());
        }
        self.appending = None;
        self.listed = None;
        self.changed = true;
        self.stamp(path, secs)
    }

    /// `OP_TRUNCATE`: the file `path` is `len` bytes long, cut or grown
    /// with zeros.
    pub fn truncate(&mut self, path: &str, len: u64) -> Result<(), Errno> {
        if len > MAX_FILE {
            return Err(libc::EFBIG);
        }
        self.changing(path);
        let mut file = self.fs()?.root_dir().open_file(path).map_err(errno)?;
        let size = fatfs::Seek::seek(&mut file, fatfs::SeekFrom::End(0)).map_err(errno)?;
        if len < size {
            fatfs::Seek::seek(&mut file, fatfs::SeekFrom::Start(len)).map_err(errno)?;
            file.truncate().map_err(errno)?;
        }
        let zeros = [0u8; 4096];
        let mut at = size;
        while at < len {
            let n = ((len - at) as usize).min(zeros.len());
            at += fatfs::Write::write(&mut file, &zeros[..n])
                .ok()
                .filter(|n| *n > 0)
                .ok_or(libc::ENOSPC)? as u64;
        }
        fatfs::Write::flush(&mut file).map_err(errno)
    }

    /// `OP_CREATE`: the file `path`, or the directory, which is not there
    /// yet. Its entry.
    pub fn create(&mut self, path: &str, dir: bool) -> Result<Entry, Errno> {
        if self.entry_at(path).is_ok() {
            return Err(libc::EEXIST);
        }
        self.changing(path);
        let root = self.fs()?.root_dir();
        if dir {
            root.create_dir(path).map(drop)
        } else {
            root.create_file(path).map(drop)
        }
        .map_err(errno)?;
        // What a listing would say of it, without one: the name as given,
        // which is how it is kept, and the clock it was stamped with.
        let now = unix_now();
        Ok(Entry {
            name: split_path(path).1.to_string(),
            is_dir: dir,
            size: 0,
            mtime: now,
            atime: now - now.rem_euclid(86_400),
            ctime: now,
        })
    }

    /// `OP_REMOVE`: the file `path`, or the directory, which is empty.
    pub fn remove(&mut self, path: &str) -> Result<(), Errno> {
        self.changing(path);
        self.fs()?.root_dir().remove(path).map_err(errno)
    }

    /// `OP_RENAME`: `from` becomes `to`, in place of the file or the empty
    /// directory that was there. Its entry under its new name.
    pub fn rename(&mut self, from: &str, to: &str) -> Result<Entry, Errno> {
        let mut moved = self.entry_at(from)?;
        self.changing(from);
        // A directory's files have other paths now.
        self.open.clear();
        let root = self.fs()?.root_dir();
        // The same entry under a name that differs in case only is not
        // something to take the place of, and the FAT code leaves it as it
        // is.
        if from.to_lowercase() == to.to_lowercase() {
            return Ok(moved);
        }
        if let Ok(there) = self.entry_at(to) {
            match (moved.is_dir, there.is_dir) {
                (true, false) => return Err(libc::ENOTDIR),
                (false, true) => return Err(libc::EISDIR),
                _ => root.remove(to).map_err(errno)?,
            }
        }
        root.rename(from, &root, to).map_err(errno)?;
        moved.name = split_path(to).1.to_string();
        Ok(moved)
    }

    /// `OP_SYNC`: everything written is on the device, and the volume is
    /// as an unmount leaves it: marked clean, its count of free clusters
    /// written. The FAT code does that only when it lets the volume go, so
    /// it is let go and opened again.
    pub fn sync(&mut self) -> Result<(), Errno> {
        self.appending = None;
        self.listed = None;
        let mut left_clean = Ok(());
        if std::mem::take(&mut self.changed) {
            if let Some(fs) = self.fs.take() {
                let fat32 = fs.fat_type() == fatfs::FatType::Fat32;
                left_clean = fs.unmount().map_err(errno);
                if self.marks_clean && left_clean.is_ok() {
                    left_clean = self.mark_clean(fat32);
                }
            }
            self.fs = Some(Box::new(Self::mount(&self.disk).map_err(errno)?));
        }
        self.flush()?;
        left_clean
    }

    /// Take the mark that the volume was not unmounted off its boot
    /// sector: the low bit of a byte whose place depends on the kind of
    /// FAT.
    fn mark_clean(&mut self, fat32: bool) -> Result<(), Errno> {
        let at = if fat32 { 0x41 } else { 0x25 };
        let mut disk = self.disk.borrow_mut();
        let mut flags = [0u8; 1];
        disk.read_exact_at(at, &mut flags).map_err(|_| libc::EIO)?;
        if flags[0] & 1 != 0 {
            flags[0] &= !1;
            disk.write_all_at(at, &flags).map_err(|_| libc::EIO)?;
        }
        Ok(())
    }

    /// The mount is over: [`sync`](Self::sync), with the count of free
    /// clusters written whatever it was. On a volume that was not left
    /// clean before this mount the FAT code does not keep the count, and
    /// what the volume says of it would stay what it was at some sync on
    /// the way.
    pub fn close(mut self) -> Result<(), Errno> {
        self.appending = None;
        // Counts them, when it has no count.
        self.fs()?.stats().map_err(errno)?;
        self.changed = true;
        self.sync()
    }

    /// `OP_STATFS`: the cluster size, the clusters, the free ones, the
    /// longest name and the magic, into `out`. The bytes written.
    pub fn statfs(&self, out: &mut [u8]) -> Result<usize, Errno> {
        let stats = self.fs()?.stats().map_err(errno)?;
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

    /// The device as it is now, read with the FAT code alone.
    fn on_device(vol: &Volume<Vec<u8>>) -> Cursor<Vec<u8>> {
        Cursor::new(vol.disk.borrow().blocks().clone())
    }

    fn file_on_device(vol: &Volume<Vec<u8>>, path: &str) -> Vec<u8> {
        let fs = fatfs::FileSystem::new(on_device(vol), FsOptions::new()).unwrap();
        let mut bytes = Vec::new();
        fs.root_dir()
            .open_file(path)
            .unwrap()
            .read_to_end(&mut bytes)
            .unwrap();
        bytes
    }

    fn read_whole(vol: &mut Volume<Vec<u8>>, path: &str) -> Vec<u8> {
        let mut bytes = Vec::new();
        let mut out = vec![0u8; 65_536];
        loop {
            let n = vol.read(path, bytes.len() as u64, &mut out).unwrap();
            if n == 0 {
                return bytes;
            }
            bytes.extend_from_slice(&out[..n]);
        }
    }

    #[test]
    fn a_file_made_and_written_reads_back_and_is_on_the_device_after_a_sync() {
        let mut vol = volume();
        let made = vol.create("docs/new file.txt", false).unwrap();
        assert_eq!(
            (made.name.as_str(), made.is_dir, made.size),
            ("new file.txt", false, 0)
        );
        assert!(made.mtime > 1_600_000_000, "stamped with the clock");
        let big = big();
        // In pieces, as the kernel sends them.
        for (i, piece) in big.chunks(65_536).enumerate() {
            let at = (i * 65_536) as u64;
            assert_eq!(vol.write("docs/new file.txt", at, piece), Ok(piece.len()));
        }
        assert_eq!(read_whole(&mut vol, "docs/new file.txt"), big);
        assert_eq!(vol.entry_at("docs/new file.txt").unwrap().size, BIG as u64);

        vol.sync().unwrap();
        assert!(!vol.is_dirty());
        assert_eq!(file_on_device(&vol, "docs/new file.txt"), big);
        // Left as an unmount leaves it.
        let fs = fatfs::FileSystem::new(on_device(&vol), FsOptions::new()).unwrap();
        assert!(!fs.read_status_flags().unwrap().dirty());
        // And it goes on after the sync.
        assert_eq!(vol.write("docs/new file.txt", BIG as u64, b"more"), Ok(4));
        assert_eq!(read_whole(&mut vol, "docs/new file.txt").len(), BIG + 4);
    }

    #[test]
    fn a_file_written_in_order_is_opened_once() {
        let mut vol = volume();
        vol.create("log", false).unwrap();
        let opens = vol.opens;
        for i in 0..50u64 {
            assert_eq!(vol.write("log", i * 1000, &[i as u8; 1000]), Ok(1000));
        }
        assert_eq!(vol.opens - opens, 1);
        let back = read_whole(&mut vol, "log");
        assert_eq!(back.len(), 50_000);
        assert!(back
            .chunks(1000)
            .enumerate()
            .all(|(i, c)| c == [i as u8; 1000]));
    }

    #[test]
    fn a_write_over_bytes_the_file_has_goes_to_where_they_are() {
        let mut vol = volume();
        let mut out = vec![0u8; 16];
        vol.read("docs/deep/big.bin", 0, &mut out).unwrap();
        let opens = vol.opens;
        for at in [0u64, 123_456, 299_000] {
            assert_eq!(vol.write("docs/deep/big.bin", at, &[0xEE; 1000]), Ok(1000));
        }
        // Whole sectors and a long piece, which goes past the kept chunks.
        assert_eq!(
            vol.write("docs/deep/big.bin", 65_536, &[0xDD; 65_536]),
            Ok(65_536)
        );
        // One more open: the entry is stamped, once for the second.
        assert!(vol.opens - opens <= 1);
        let mut expect = big();
        for at in [0usize, 123_456, 299_000] {
            expect[at..at + 1000].fill(0xEE);
        }
        expect[65_536..131_072].fill(0xDD);
        assert_eq!(read_whole(&mut vol, "docs/deep/big.bin"), expect);
        vol.sync().unwrap();
        assert_eq!(file_on_device(&vol, "docs/deep/big.bin"), expect);
    }

    #[test]
    fn a_write_past_the_end_leaves_zeros_before_it() {
        let mut vol = volume();
        assert_eq!(vol.write("hello.txt", 10_000, b"far"), Ok(3));
        let back = read_whole(&mut vol, "hello.txt");
        assert_eq!(back.len(), 10_003);
        assert_eq!(&back[..13], b"hello, world\n");
        assert!(back[13..10_000].iter().all(|b| *b == 0));
        assert_eq!(&back[10_000..], b"far");
        // A write that starts inside and ends outside.
        assert_eq!(vol.write("hello.txt", 9_999, b"nearer"), Ok(6));
        assert_eq!(&read_whole(&mut vol, "hello.txt")[9_999..], b"nearer");
        assert_eq!(vol.write("no-such-file", 0, b"x"), Err(libc::ENOENT));
        assert_eq!(
            vol.write("hello.txt", u32::MAX as u64, b"x"),
            Err(libc::EFBIG)
        );
    }

    #[test]
    fn a_file_is_cut_and_grown() {
        let mut vol = volume();
        vol.truncate("docs/deep/big.bin", 100_000).unwrap();
        assert_eq!(read_whole(&mut vol, "docs/deep/big.bin"), big()[..100_000]);
        vol.truncate("docs/deep/big.bin", 100_500).unwrap();
        let back = read_whole(&mut vol, "docs/deep/big.bin");
        assert_eq!(back.len(), 100_500);
        assert!(back[100_000..].iter().all(|b| *b == 0));
        vol.truncate("docs/deep/big.bin", 0).unwrap();
        assert_eq!(read_whole(&mut vol, "docs/deep/big.bin"), []);
        // And written again from nothing.
        assert_eq!(vol.write("docs/deep/big.bin", 0, b"again"), Ok(5));
        vol.sync().unwrap();
        assert_eq!(file_on_device(&vol, "docs/deep/big.bin"), b"again");
        assert_eq!(vol.truncate("nowhere", 0), Err(libc::ENOENT));
    }

    #[test]
    fn what_is_made_twice_or_badly_named_is_refused() {
        let mut vol = volume();
        assert_eq!(vol.create("hello.txt", false).unwrap_err(), libc::EEXIST);
        // FAT's names are one whatever their case.
        assert_eq!(vol.create("HELLO.TXT", true).unwrap_err(), libc::EEXIST);
        assert_eq!(vol.create("nowhere/file", false).unwrap_err(), libc::ENOENT);
        assert_eq!(vol.create("what?.txt", false).unwrap_err(), libc::EINVAL);
        let dir = vol.create("docs/sub", true).unwrap();
        assert!(dir.is_dir);
        vol.create("docs/sub/inside", false).unwrap();
        assert_eq!(
            names(&mut vol, "docs/sub"),
            [("inside".to_string(), false, 0)]
        );
    }

    #[test]
    fn a_file_or_an_empty_directory_is_removed() {
        let mut vol = volume();
        let mut out = vec![0u8; 16];
        vol.read("hello.txt", 0, &mut out).unwrap();
        vol.remove("hello.txt").unwrap();
        assert_eq!(vol.read("hello.txt", 0, &mut out), Err(libc::ENOENT));
        assert_eq!(vol.remove("hello.txt"), Err(libc::ENOENT));
        assert_eq!(vol.remove("docs"), Err(libc::ENOTEMPTY));
        vol.remove("docs/A long name with spaces.md").unwrap();
        vol.remove("docs/deep/big.bin").unwrap();
        vol.remove("docs/deep").unwrap();
        vol.remove("docs").unwrap();
        assert_eq!(names(&mut vol, ""), [("many".to_string(), true, 0)]);
        // The name is free for another file, which is not the old one.
        vol.create("hello.txt", false).unwrap();
        assert_eq!(vol.read("hello.txt", 0, &mut out), Ok(0));
    }

    #[test]
    fn a_rename_moves_and_takes_the_place_of_what_was_there() {
        let mut vol = volume();
        let mut out = vec![0u8; 64];
        vol.read("hello.txt", 0, &mut out).unwrap();
        let moved = vol.rename("hello.txt", "docs/greeting.txt").unwrap();
        assert_eq!((moved.name.as_str(), moved.size), ("greeting.txt", 13));
        assert_eq!(vol.read("hello.txt", 0, &mut out), Err(libc::ENOENT));
        assert_eq!(vol.read("docs/greeting.txt", 0, &mut out), Ok(13));
        // Over a file that is there.
        vol.rename("docs/greeting.txt", "docs/A long name with spaces.md")
            .unwrap();
        assert_eq!(
            vol.read("docs/a long name with spaces.md", 0, &mut out),
            Ok(13)
        );
        assert_eq!(&out[..13], b"hello, world\n");
        assert_eq!(names(&mut vol, "docs").len(), 2);
        // A directory moves with what is in it.
        vol.read("docs/deep/big.bin", 0, &mut out).unwrap();
        let moved = vol.rename("docs/deep", "abyss").unwrap();
        assert!(moved.is_dir);
        assert_eq!(read_whole(&mut vol, "abyss/big.bin"), big());
        assert_eq!(
            vol.read("docs/deep/big.bin", 0, &mut out),
            Err(libc::ENOENT)
        );
        // What cannot take the place of what.
        assert_eq!(
            vol.rename("abyss", "docs/A long name with spaces.md")
                .unwrap_err(),
            libc::ENOTDIR
        );
        assert_eq!(
            vol.rename("docs/A long name with spaces.md", "abyss")
                .unwrap_err(),
            libc::EISDIR
        );
        assert_eq!(vol.rename("many", "abyss").unwrap_err(), libc::ENOTEMPTY);
        assert_eq!(
            vol.rename("nothing", "something").unwrap_err(),
            libc::ENOENT
        );
        // An empty directory is taken the place of.
        vol.create("empty", true).unwrap();
        vol.rename("abyss", "empty").unwrap();
        assert_eq!(read_whole(&mut vol, "empty/big.bin"), big());
        vol.sync().unwrap();
        assert_eq!(file_on_device(&vol, "empty/big.bin"), big());
    }

    #[test]
    fn a_volume_that_fills_up_says_so_and_stays_whole() {
        let mut vol = volume();
        vol.create("filler", false).unwrap();
        let piece = vec![0x5Au8; 65_536];
        let mut at = 0u64;
        let last = loop {
            match vol.write("filler", at, &piece) {
                Ok(n) if n == piece.len() => at += n as u64,
                other => break other,
            }
        };
        // The last piece is written in part, or not at all.
        match last {
            Ok(n) => {
                at += n as u64;
                assert_eq!(vol.write("filler", at, &piece), Err(libc::ENOSPC));
            }
            Err(e) => assert_eq!(e, libc::ENOSPC),
        }
        assert!(at > 6 << 20, "{at}");
        assert_eq!(vol.entry_at("filler").unwrap().size, at);
        vol.sync().unwrap();
        assert_eq!(file_on_device(&vol, "filler").len() as u64, at);
        // Room again once it is gone.
        vol.remove("filler").unwrap();
        vol.create("after", false).unwrap();
        assert_eq!(vol.write("after", 0, &piece), Ok(piece.len()));
    }

    #[test]
    fn a_volume_that_was_not_left_clean_ends_with_its_free_clusters_counted() {
        // FAT32, which is what keeps a count: 40 MiB of 512-byte clusters.
        let mut image = Cursor::new(vec![0u8; 40 << 20]);
        let options = fatfs::FormatVolumeOptions::new()
            .fat_type(fatfs::FatType::Fat32)
            .bytes_per_cluster(512);
        fatfs::format_volume(&mut StdIoWrapper::new(&mut image), options).unwrap();
        let mut image = image.into_inner();
        let free = {
            let fs = fatfs::FileSystem::new(Cursor::new(&mut image), FsOptions::new()).unwrap();
            fs.stats().unwrap().free_clusters()
        };
        // Marked as not unmounted, the way a server that was killed
        // leaves it.
        image[0x41] |= 1;
        let len = image.len() as u64;
        let mut vol = Volume::open(image, len).unwrap();
        vol.create("file", false).unwrap();
        assert_eq!(vol.write("file", 0, &[1; 60_000]), Ok(60_000));
        vol.sync().unwrap();
        vol.remove("file").unwrap();
        let disk = vol.disk.clone();
        vol.close().unwrap();
        // The count in the FSInfo sector, which is the second.
        let image = disk.borrow().blocks().clone();
        let counted = u32::from_le_bytes(image[512 + 488..512 + 492].try_into().unwrap());
        assert_eq!(counted, free);
    }

    #[test]
    fn the_time_of_last_writing_is_what_it_is_set_to() {
        let mut vol = volume();
        let at = 1_709_164_800 + 13 * 3600 + 14 * 60 + 16;
        vol.set_time("hello.txt", at).unwrap();
        assert_eq!(vol.entry_at("hello.txt").unwrap().mtime, at);
        // A directory's is left as it is, and that is not an error.
        vol.set_time("docs", at).unwrap();
        assert_eq!(vol.set_time("nowhere", at), Err(libc::ENOENT));
    }

    #[test]
    fn seconds_since_1970_are_a_date_and_back() {
        for secs in [315_532_800i64, 1_709_164_800, 1_709_251_199, 4_102_444_800] {
            // FAT's seconds are even.
            let even = secs / 2 * 2;
            assert_eq!(unix_from_date_time(date_time_from_unix(even)), even);
        }
        // Outside what FAT has: its first day.
        assert_eq!(unix_from_date_time(date_time_from_unix(0)), 315_532_800);
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

    #[test]
    fn the_next_server_goes_on_from_what_the_dead_one_was_told() {
        let mut memory = vec![0u128; Kept::LEN / 16];
        let base = memory.as_mut_ptr().cast::<u8>();
        let (image, len) = {
            let vol = volume();
            let image = vol.disk.borrow().blocks().clone();
            (image.clone(), image.len() as u64)
        };
        // SAFETY: the test's own memory, one volume over it at a time.
        let mut vol = Volume::open_over(image, len, unsafe { Kept::at(base) }).unwrap();
        let big = big();
        vol.create("docs/new.bin", false).unwrap();
        assert_eq!(vol.write("docs/new.bin", 0, &big[..40_000]), Ok(40_000));
        assert_eq!(vol.write("hello.txt", 7, b"there\n"), Ok(6));
        vol.rename("docs/deep/big.bin", "moved.bin").unwrap();
        vol.remove("many/file-number-0007.dat").unwrap();
        // The server dies as it is, with none of that on the device.
        let device = vol.disk.borrow().blocks().clone();
        core::mem::forget(vol);
        assert!(Volume::open(device.clone(), len)
            .unwrap()
            .entries("docs")
            .unwrap()
            .iter()
            .all(|e| e.name != "new.bin"));

        // SAFETY: as above; the first is gone.
        let mut next = Volume::open_over(device, len, unsafe { Kept::at(base) }).unwrap();
        let mut out = vec![0u8; 65_536];
        assert_eq!(next.read("docs/new.bin", 0, &mut out), Ok(40_000));
        assert_eq!(out[..40_000], big[..40_000]);
        assert_eq!(next.read("hello.txt", 0, &mut out), Ok(13));
        assert_eq!(&out[..13], b"hello, there\n");
        assert_eq!(next.read("moved.bin", 0, &mut out[..100]), Ok(100));
        assert!(next.entries("docs/deep").unwrap().is_empty());
        assert_eq!(next.entries("many").unwrap().len(), 299);
        // And goes on: what the kernel asks twice is done once.
        assert_eq!(
            next.write("docs/new.bin", 40_000, &big[40_000..50_000]),
            Ok(10_000)
        );
        assert_eq!(
            next.write("docs/new.bin", 40_000, &big[40_000..50_000]),
            Ok(10_000)
        );
        let disk = next.disk.clone();
        next.close().unwrap();
        // The device alone has all of it now, and the volume is whole.
        let device = disk.borrow().blocks().clone();
        let mut alone = Volume::open(device, len).unwrap();
        assert!(!alone.fs().unwrap().read_status_flags().unwrap().dirty());
        assert_eq!(alone.read("docs/new.bin", 0, &mut out), Ok(50_000));
        assert_eq!(out[..50_000], big[..50_000]);
        let mut numbers = [0u8; 40];
        alone.statfs(&mut numbers).unwrap();
    }
}
