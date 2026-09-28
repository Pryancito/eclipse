use {
    super::*,
    crate::object::*,
    alloc::sync::Arc,
    core::sync::atomic::{AtomicU32, Ordering},
    kernel_hal::sync::Mutex,
    numeric_enum_macro::numeric_enum,
};

/// A readable, writable, seekable interface to some underlying storage
///
/// ## SYNOPSIS
///
/// A stream is an interface for reading and writing data to some underlying
/// storage, typically a VMO.
pub struct Stream {
    base: KObjectBase,
    options: AtomicU32,
    vmo: Arc<VmObject>,
    seek: Mutex<usize>,
}

bitflags::bitflags! {
    pub struct StreamOptions: u32 {
        const MODE_READ = 1 << 0;
        const MODE_WRITE = 1 << 1;
        const MODE_APPEND = 1 << 2;
    }
}

impl_kobject!(Stream);

numeric_enum! {
    #[repr(usize)]
    #[derive(Debug)]
    /// Enumeration of possible methods to modify the seek within an Stream.
    pub enum SeekOrigin {
        /// Set the seek offset relative to the start of the stream.
        Start = 0,
        /// Set the seek offset relative to the current seek offset of the stream.
        Current = 1,
        /// Set the seek offset relative to the end of the stream, as defined by the content size of the stream.
        End = 2,
    }
}

impl Stream {
    /// Create a stream from a VMO
    pub fn create(vmo: Arc<VmObject>, seek: usize, options: u32) -> Arc<Self> {
        Arc::new(Stream {
            base: KObjectBase::default(),
            options: AtomicU32::new(options),
            vmo,
            seek: Mutex::new(seek),
        })
    }

    /// Read data from the stream at the current seek offset
    pub fn read(&self, data: &mut [u8]) -> ZxResult<usize> {
        let mut seek = self.seek.lock();
        let length = self.read_at(data, *seek)?;
        *seek += length;
        Ok(length)
    }

    /// Read data from the stream at a given offset
    pub fn read_at(&self, data: &mut [u8], offset: usize) -> ZxResult<usize> {
        let count = data.len();
        let content_size = self.vmo.content_size();
        if offset >= content_size {
            return Ok(0);
        }
        let length = count.min(content_size - offset);
        self.vmo.read(offset, &mut data[..length])?;
        Ok(length)
    }

    /// write data to the stream at the current seek offset or append data at the end of content
    pub fn write(&self, data: &[u8], append: bool) -> ZxResult<usize> {
        let mut seek = self.seek.lock();
        if data.is_empty() {
            return Ok(0);
        }
        let offset = if append || self.append_mode() {
            None
        } else {
            Some(*seek)
        };
        let (offset, length) = self.vmo.write_stream(offset, data)?;
        *seek = offset + length;
        Ok(length)
    }

    /// Validate the size calculation before touching any user buffers.
    pub fn check_write_size(&self, count: usize, append: bool, offset: Option<usize>) -> ZxResult {
        if count == 0 {
            return Ok(());
        }
        let append = offset.is_none() && (append || self.append_mode());
        let offset = match offset {
            Some(offset) => offset,
            None if append => self.vmo.content_size(),
            None => *self.seek.lock(),
        };
        offset.checked_add(count).ok_or(if append {
            ZxError::OUT_OF_RANGE
        } else {
            ZxError::FILE_BIG
        })?;
        Ok(())
    }

    /// Write data to the stream at a given offset
    pub fn write_at(&self, data: &[u8], offset: usize) -> ZxResult<usize> {
        self.vmo
            .write_stream(Some(offset), data)
            .map(|(_, length)| length)
    }

    /// Modify the current seek offset of the stream
    pub fn seek(&self, whence: SeekOrigin, offset: isize) -> ZxResult<usize> {
        let mut seek = self.seek.lock();
        let origin: usize = match whence {
            SeekOrigin::Start => 0,
            SeekOrigin::Current => *seek,
            SeekOrigin::End => self.vmo.content_size(),
        };
        *seek = if offset >= 0 {
            origin
                .checked_add(offset as usize)
                .ok_or(ZxError::INVALID_ARGS)?
        } else {
            origin
                .checked_sub(offset.unsigned_abs())
                .ok_or(ZxError::INVALID_ARGS)?
        };
        Ok(*seek)
    }

    /// Return whether writes without an explicit option append to the VMO.
    pub fn append_mode(&self) -> bool {
        self.options.load(Ordering::Relaxed) & StreamOptions::MODE_APPEND.bits() != 0
    }

    /// Change the persistent append mode of this stream.
    pub fn set_append_mode(&self, append: bool) {
        if append {
            self.options
                .fetch_or(StreamOptions::MODE_APPEND.bits(), Ordering::Relaxed);
        } else {
            self.options
                .fetch_and(!StreamOptions::MODE_APPEND.bits(), Ordering::Relaxed);
        }
    }

    /// Get information of the socket.
    pub fn get_info(&self) -> StreamInfo {
        let seek = self.seek.lock();
        StreamInfo {
            options: self.options.load(Ordering::Relaxed),
            padding1: 0,
            seek: *seek as u64,
            content_size: self.vmo.content_size() as u64,
        }
    }
}

/// Information of a Stream
#[repr(C)]
#[derive(Default)]
pub struct StreamInfo {
    /// The options passed to `Stream::create()`.
    options: u32,
    padding1: u32,
    /// The current seek offset.
    ///
    /// Used by stream_readv and stream_writev to determine where to read
    /// and write the stream.
    seek: u64,
    /// The current size of the stream.
    ///
    /// The number of bytes in the stream that store data. The stream itself
    /// might have a larger capacity to avoid reallocating the underlying storage
    /// as the stream grows or shrinks.
    /// NOTE: in fact, this value is store in the VmObject associated and can be
    /// get/set through 'object_[get/set]_property(vmo_handle, ...)'
    content_size: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vm::PAGE_SIZE;

    #[async_std::test]
    async fn separate_streams_append_without_overwriting() {
        let vmo = VmObject::new_paged(1);
        vmo.set_content_size(0).unwrap();
        let mut writers = alloc::vec::Vec::new();
        for id in 0..4u8 {
            let stream = Stream::create(vmo.clone(), 0, StreamOptions::MODE_APPEND.bits());
            writers.push(async_std::task::spawn_blocking(move || {
                for _ in 0..256 {
                    assert_eq!(stream.write(&[id], false).unwrap(), 1);
                }
            }));
        }
        for writer in writers {
            writer.await;
        }
        assert_eq!(vmo.content_size(), 1024);
        let mut data = [0u8; 1024];
        vmo.read(0, &mut data).unwrap();
        for id in 0..4u8 {
            assert_eq!(data.iter().filter(|byte| **byte == id).count(), 256);
        }
    }

    /// A one-page VMO holding `content` bytes of content, and a stream over it
    /// positioned at `seek`.
    fn stream_with(content: usize, seek: usize, options: u32) -> (Arc<VmObject>, Arc<Stream>) {
        let vmo = VmObject::new_paged(1);
        vmo.write(0, &[0xcd; PAGE_SIZE]).unwrap();
        vmo.set_content_size(content).unwrap();
        let stream = Stream::create(vmo.clone(), seek, options);
        (vmo, stream)
    }

    /// The VMO is a whole page; the content size says how much of it is data.
    /// A read that runs off the end of the content must stop there, because
    /// what is past it is whatever the page held before -- another process's
    /// bytes, if the page was recycled -- and the stream is how userspace
    /// reads a VMO.
    #[test]
    fn a_read_stops_at_the_content_size_and_not_at_the_end_of_the_vmo() {
        let (_vmo, stream) = stream_with(10, 0, 0);

        let mut buf = [0xabu8; 64];
        assert_eq!(
            stream.read(&mut buf).unwrap(),
            10,
            "diez bytes de contenido son diez bytes de lectura"
        );
        assert!(
            buf[10..].iter().all(|&b| b == 0xab),
            "y nada mas alla del contenido llega al buffer del que lee"
        );

        // And at the end there is nothing left, however much is asked for.
        assert_eq!(stream.read(&mut buf).unwrap(), 0);
        assert_eq!(stream.read_at(&mut buf, 10).unwrap(), 0);
        assert_eq!(stream.read_at(&mut buf, usize::MAX).unwrap(), 0);
    }

    /// The cursor moves by what came back, not by what was asked for. Moving
    /// it by the request leaves it past the content after the first short read
    /// at the end of the file, and every read after that answers zero on a
    /// stream that still has data behind the cursor.
    #[test]
    fn the_cursor_moves_by_what_was_read_and_not_by_what_was_asked_for() {
        let (_vmo, stream) = stream_with(10, 0, 0);

        let mut buf = [0u8; 4];
        assert_eq!(stream.read(&mut buf).unwrap(), 4);
        let info = stream.get_info();
        assert_eq!((info.seek, info.content_size), (4, 10), "cursor y tamano");

        assert_eq!(stream.read(&mut buf).unwrap(), 4);
        assert_eq!(stream.get_info().seek, 8);

        // The short read at the end moves it by two, not by four.
        assert_eq!(stream.read(&mut buf).unwrap(), 2);
        assert_eq!(stream.get_info().seek, 10);
        assert_eq!(stream.read(&mut buf).unwrap(), 0);
        assert_eq!(stream.get_info().seek, 10, "y ahi se queda");
    }

    /// `zx_stream_create` takes the offset to start at, and `fdopen` on an
    /// already-open file is where it comes from: a stream that always started
    /// at zero would re-read the file from the top.
    #[test]
    fn a_stream_starts_where_it_was_told_to_and_not_at_the_top() {
        let (_vmo, stream) = stream_with(10, 6, 0);

        assert_eq!(stream.get_info().seek, 6);
        let mut buf = [0u8; 64];
        assert_eq!(
            stream.read(&mut buf).unwrap(),
            4,
            "quedan cuatro bytes desde el seis"
        );
    }

    /// The three origins of `zx_stream_seek` measure from three different
    /// places, and `End` measures from the content size rather than from the
    /// VMO: `lseek(fd, 0, SEEK_END)` is how every program asks how big a file
    /// is, and answering the page size instead says a ten-byte file is 4096.
    #[test]
    fn the_three_seek_origins_measure_from_three_different_places() {
        let (_vmo, stream) = stream_with(100, 0, 0);

        assert_eq!(stream.seek(SeekOrigin::Start, 10).unwrap(), 10);
        assert_eq!(
            stream.seek(SeekOrigin::Current, 5).unwrap(),
            15,
            "Current parte de donde esta el cursor"
        );
        assert_eq!(
            stream.seek(SeekOrigin::End, 0).unwrap(),
            100,
            "End parte del tamano del contenido, no del de la VMO"
        );
        assert_eq!(
            stream.seek(SeekOrigin::End, -20).unwrap(),
            80,
            "y un desplazamiento negativo resta"
        );
        assert_eq!(stream.seek(SeekOrigin::Current, -80).unwrap(), 0);
    }

    /// A seek that would leave the number line is an error, and an error that
    /// has already moved the cursor is worse than the seek: the next read
    /// comes from somewhere nobody asked for. `usize` wraps, so without the
    /// check the answer is a huge offset rather than a refusal.
    #[test]
    fn a_seek_that_leaves_the_number_line_is_refused() {
        let (_vmo, stream) = stream_with(100, 0, 0);

        assert_eq!(stream.seek(SeekOrigin::Start, 0).unwrap(), 0);
        assert_eq!(
            stream.seek(SeekOrigin::Current, -1),
            Err(ZxError::INVALID_ARGS),
            "antes del principio no hay nada"
        );
        assert_eq!(stream.get_info().seek, 0, "y el cursor no se ha movido");

        assert_eq!(
            stream.seek(SeekOrigin::Start, isize::MAX).unwrap(),
            isize::MAX as usize
        );
        let far = stream.seek(SeekOrigin::Current, isize::MAX).unwrap();
        assert_eq!(
            far,
            usize::MAX - 1,
            "dos veces el maximo con signo, menos uno"
        );
        assert_eq!(
            stream.seek(SeekOrigin::Current, 2),
            Err(ZxError::INVALID_ARGS),
            "y mas alla del ultimo numero tampoco"
        );
        assert_eq!(stream.get_info().seek, far as u64, "sigue donde estaba");
    }

    /// The two ways a write can be too big have two different names in the
    /// Zircon ABI, and the check runs BEFORE any user buffer is touched. An
    /// append is measured from the end of the content, so a stream with
    /// content in it overflows where an empty one would not.
    #[test]
    fn a_write_that_would_not_fit_is_refused_by_the_name_the_abi_gives_it() {
        let (_vmo, stream) = stream_with(1, 0, 0);

        assert_eq!(stream.check_write_size(1, false, Some(0)), Ok(()));
        assert_eq!(
            stream.check_write_size(usize::MAX, true, None),
            Err(ZxError::OUT_OF_RANGE),
            "un append que se sale es OUT_OF_RANGE, y se mide desde el final \
             del contenido"
        );
        assert_eq!(
            stream.check_write_size(usize::MAX, false, Some(1)),
            Err(ZxError::FILE_BIG),
            "y uno posicionado es FILE_BIG"
        );
        assert_eq!(
            stream.check_write_size(usize::MAX - 1, false, Some(1)),
            Ok(()),
            "justo lo que cabe en el numero, cabe"
        );
    }

    /// `zx_stream_set_options` changes one bit. The read and write modes are
    /// in the same word, and a stream that loses them is a stream nothing can
    /// use again.
    #[test]
    fn turning_append_on_and_off_leaves_the_other_options_alone() {
        let both = StreamOptions::MODE_READ.bits() | StreamOptions::MODE_WRITE.bits();
        let (_vmo, stream) = stream_with(10, 0, both);

        assert!(!stream.append_mode(), "el append no estaba pedido");

        stream.set_append_mode(true);
        assert!(stream.append_mode());
        assert_eq!(
            stream.get_info().options & both,
            both,
            "lectura y escritura siguen ahi"
        );

        stream.set_append_mode(false);
        assert!(!stream.append_mode());
        assert_eq!(stream.get_info().options & both, both);
    }

    /// After a write the cursor is where the write ENDED, not how long it
    /// was. Leaving it at the length points it back at the start of what was
    /// just written whenever the write did not begin at zero, so the next
    /// write overwrites the last one and a program appending records writes
    /// the same slot for ever.
    #[test]
    fn the_cursor_after_a_write_is_where_the_write_ended() {
        let (_vmo, stream) = stream_with(10, 3, 0);

        assert_eq!(stream.write(&[1, 2, 3, 4], false).unwrap(), 4);
        assert_eq!(
            stream.get_info().seek,
            7,
            "tres de donde empezo mas cuatro escritos"
        );

        // An append ignores the cursor and lands at the end of the content,
        // and the cursor follows it there.
        assert_eq!(stream.write(&[9], true).unwrap(), 1);
        let info = stream.get_info();
        assert_eq!((info.seek, info.content_size), (11, 11));
    }

    /// `write(fd, buf, 0)` is a legal call that does nothing, and on an append
    /// stream "nothing" includes not moving the cursor to the end of the file.
    #[test]
    fn a_write_of_no_bytes_leaves_the_cursor_where_it_was() {
        let (_vmo, stream) = stream_with(10, 3, StreamOptions::MODE_APPEND.bits());

        assert_eq!(stream.write(&[], false).unwrap(), 0);
        assert_eq!(stream.get_info().seek, 3);
        assert_eq!(stream.write(&[], true).unwrap(), 0);
        assert_eq!(stream.get_info().seek, 3);
    }
}
