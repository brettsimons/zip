use crate::error;
use core::marker::PhantomData;
#[cfg(feature = "std")]
use std::io;

mod decrypt;
pub use decrypt::{Decrypt, DecryptBuilder};

/// An [`io::Read`] implementation for files stored in a ZIP archive.
///
/// Can be created using [`super::File::reader`]
pub struct Read<'a, D, Buffered = D>(ReadImpl<'a, D, Buffered>);

/// A cache for the state of [`Read`]s
///
/// When a [`super::File`] is being extracted, it uses large buffers to keep
/// track of the decompression dictionary.
///
/// To avoid the cost of initializing these buffers, you can preallocate them
/// with [`Store::default`] and feed it into [`super::File::reader`] when
/// extracting your files.
#[derive(Default)]
pub struct Store {
    #[cfg(feature = "read-deflate")]
    deflate: Option<(miniz_oxide::inflate::core::DecompressorOxide, InflateBuffer)>,
}

pub struct Decrypted(());
pub struct Found(());
pub struct Not<T>(T);
pub struct ReadBuilder<D, St = (Not<Decrypted>, Not<Found>)> {
    disk: D,
    storage: FileStorage,
    state: PhantomData<St>,
}

enum ReadImpl<'a, D, Buffered> {
    Never {
        never: core::convert::Infallible,
        b: Buffered,
        stored: &'a mut (),
    },
    Stored {
        disk: D,
        remaining: u64,
    },
    #[cfg(feature = "read-deflate")]
    Deflate {
        disk: Buffered,
        remaining: u64,
        decompressor: &'a mut (miniz_oxide::inflate::core::DecompressorOxide, InflateBuffer),
        out_pos: u16,
        read_cursor: u16,
    },
}

impl<D> ReadBuilder<D> {
    pub(super) fn new(
        disk: D,
        header: super::FileLocator,
    ) -> Result<Self, error::MethodNotSupported> {
        Ok(Self {
            disk,
            storage: header.storage.ok_or(error::MethodNotSupported(()))?,
            state: PhantomData,
        })
    }
}

impl<D> ReadBuilder<D, (Not<Decrypted>, Not<Found>)> {
    pub fn without_encryption(self) -> Result<ReadBuilder<D, (Decrypted, Not<Found>)>, error::FileLocked> {
        (!self.storage.encrypted)
            .then(|| ReadBuilder {
                disk: self.disk,
                storage: self.storage,
                state: PhantomData,
            })
            .ok_or(error::FileLocked(()))
    }
}

#[cfg(feature = "std")]
impl<D: io::Seek + io::Read, E> ReadBuilder<D, (E, Not<Found>)> {
    pub fn seek_to_data(mut self) -> io::Result<ReadBuilder<D, (E, Found)>> {
        // TODO: avoid seeking if we can, since this will often be done in a loop
        self.disk
            .seek(std::io::SeekFrom::Start(self.storage.start))?;
        let mut buf = [0; std::mem::size_of::<zip_format::Header>() + 4];
        self.disk.read_exact(&mut buf)?;
        let header = zip_format::Header::as_prefix(&buf).ok_or(error::NotAnArchive(()))?;
        self.disk.seek(std::io::SeekFrom::Current(
            header.name_len.get() as i64 + header.metadata_len.get() as i64,
        ))?;
        Ok(ReadBuilder {
            disk: self.disk,
            storage: self.storage,
            state: PhantomData,
        })
    }
}
#[cfg(feature = "std")]
impl<D: io::Read> ReadBuilder<D, (Not<Decrypted>, Found)> {
    pub fn unlock_io(self) -> io::Result<Result<ReadBuilder<D, (Decrypted, Found)>, decrypt::DecryptBuilder<D>>> {
        if !self.storage.encrypted {
            Ok(Ok(ReadBuilder {
                disk: self.disk,
                storage: self.storage,
                state: PhantomData,
            }))
        } else {
            decrypt::DecryptBuilder::from_io(self).map(Err)
        }
    }
}
impl<D> ReadBuilder<D, (Decrypted, Found)> {
    pub fn build_io<Buffered>(
        self,
        store: &mut Store,
        f: impl FnOnce(D) -> Buffered,
    ) -> Read<'_, D, Buffered> {
        Read(match self.storage.kind {
            FileStorageKind::Stored => ReadImpl::Stored {
                remaining: self.storage.len,
                disk: self.disk,
            },
            #[cfg(feature = "read-deflate")]
            FileStorageKind::Deflated => ReadImpl::Deflate {
                disk: f(self.disk),
                remaining: if self.storage.unknown_size {
                    u64::MAX
                } else {
                    self.storage.len
                },
                decompressor: {
                    let deflate = store.deflate.get_or_insert_with(Default::default);
                    deflate.0.init();
                    deflate
                },
                out_pos: 0,
                read_cursor: 0,
            },
        })
    }
}

#[cfg(feature = "std")]
impl<D: io::Read, Buffered: io::BufRead> io::Read for Read<'_, D, Buffered> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match &mut self.0 {
            ReadImpl::Never { never, .. } => match *never {},
            ReadImpl::Stored { disk, remaining } => {
                let n = disk.take(*remaining).read(buf)?;
                *remaining -= n as u64;
                Ok(n)
            }
            #[cfg(feature = "read-deflate")]
            ReadImpl::Deflate {
                disk,
                decompressor,
                out_pos,
                read_cursor,
                ..
            } => {
                // TODO: track `remaining`
                // TODO: Check the CRC of the decompressed data
                let (decompressor, InflateBuffer(backbuf)) = decompressor;
                let mut out = *out_pos as usize;
                let mut cursor = *read_cursor as usize;
                if read_cursor == out_pos {
                    let data = disk.fill_buf()?;
                    if out == backbuf.len() {
                        out = 0;
                    }
                    let (_status, consumed, written) =
                        miniz_oxide::inflate::core::decompress(decompressor, data, backbuf, out, 0);
                    out += written;
                    disk.consume(consumed);
                }
                if cursor == backbuf.len() {
                    cursor = 0;
                }
                let len = out.checked_sub(cursor).unwrap().min(buf.len());
                buf[..len].copy_from_slice(&backbuf[cursor..][..len]);
                *read_cursor = (cursor + len) as u16;
                *out_pos = out as u16;
                Ok(len)
            }
        }
    }
}

struct InflateBuffer([u8; 32 * 1024]);
impl Default for InflateBuffer {
    fn default() -> Self {
        Self([0; 32 * 1024])
    }
}

/// We use an `Option<FileStorage>` in [`super::FileLocator`] to represent files that we might not be able to read.
pub(crate) struct FileStorage {
    pub(crate) start: u64,
    pub(crate) len: u64,
    pub(crate) crc32: u32,
    pub(crate) encrypted: bool,
    pub(crate) unknown_size: bool,
    pub(crate) kind: FileStorageKind,
}

/// List of available [`zip_format::CompressionMethod`] implementations.
pub(crate) enum FileStorageKind {
    Stored,
    #[cfg(feature = "read-deflate")]
    Deflated,
}
