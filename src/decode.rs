extern crate constant_time_eq;
extern crate either;

use self::constant_time_eq::constant_time_eq;
use self::either::Either::{self, Left, Right};
use arrayvec::ArrayVec;

use encode;
use hash::Finalization::{self, NotRoot, Root};
use hash::{self, Hash, CHUNK_SIZE, HASH_SIZE, HEADER_SIZE, MAX_DEPTH, PARENT_SIZE};

use std;
use std::cmp;
use std::io;
use std::io::prelude::*;

#[derive(Clone)]
pub struct State {
    stack: ArrayVec<[Subtree; MAX_DEPTH]>,
    root_hash: Hash,
    content_length: Option<u64>,
    length_verified: bool,
    content_position: u64,
    encoded_offset: u128,
}

impl State {
    pub fn new(root_hash: Hash) -> Self {
        Self {
            stack: ArrayVec::new(),
            root_hash,
            content_length: None,
            length_verified: false,
            content_position: 0,
            encoded_offset: 0,
        }
    }

    pub fn position(&self) -> u64 {
        self.content_position
    }

    fn reset_to_root(&mut self) {
        self.content_position = 0;
        self.encoded_offset = HEADER_SIZE as u128;
        self.stack.clear();
        self.stack.push(Subtree {
            hash: self.root_hash,
            start: 0,
            end: self.content_length.expect("no header"),
        });
    }

    pub fn read_next(&self) -> StateNext {
        let content_length;
        match self.len_next() {
            Left(len) => content_length = len,
            Right(next) => return next,
        }
        if let Some(subtree) = self.stack.last() {
            subtree.state_next(content_length, self.content_position)
        } else {
            assert!(self.length_verified, "unverified EOF");
            StateNext::Done
        }
    }

    /// Note that if reading the length returns StateNext::Chunk (leading the caller to call
    /// feed_subtree), the content position will no longer be at the start, as with a standard
    /// read. Callers that don't buffer the last read chunk (as Reader does) might need to do an
    /// additional seek to compensate.
    pub fn len_next(&self) -> Either<u64, StateNext> {
        if let Some(content_length) = self.content_length {
            if self.length_verified {
                Left(content_length)
            } else {
                let current_subtree = *self.stack.last().expect("unverified EOF");
                let next = current_subtree.state_next(content_length, self.content_position);
                Right(next)
            }
        } else {
            Right(StateNext::Header)
        }
    }

    pub fn seek_next(&mut self, content_position: u64) -> (u128, StateNext) {
        // Get the current content length. This will lead us to read the header and verify the root
        // node, if we haven't already.
        let content_length;
        match self.len_next() {
            Left(len) => content_length = len,
            Right(next) => return (self.encoded_offset, next),
        }

        // Record the target position, which we use in read_next() to compute the skip.
        self.content_position = content_position;

        // If we're already past EOF, either reset or short circuit.
        if self.stack.is_empty() {
            if content_position >= content_length {
                return (self.encoded_offset, StateNext::Done);
            } else {
                self.reset_to_root();
            }
        }

        // Also reset if we're in the tree but the seek is to our left.
        if content_position < self.stack.last().unwrap().start {
            self.reset_to_root();
        }

        // The main loop. Pop subtrees out of the stack until we find one that contains the seek
        // target, and then descend into that tree. Repeat (through in subsequent calls) until the
        // next chunk contains the seek target, or until we hit EOF.
        while let Some(&current_subtree) = self.stack.last() {
            // If the target is within the next chunk, the seek is finished. Note that there may be
            // more parent nodes in front of the chunk, but read will process them as usual.
            if content_position < current_subtree.start + CHUNK_SIZE as u64 {
                return (self.encoded_offset, StateNext::Done);
            }

            // If the target is outside the next chunk, but within the current subtree, then we
            // need to descend.
            if content_position < current_subtree.end {
                return (
                    self.encoded_offset,
                    current_subtree.state_next(content_length, self.content_position),
                );
            }

            // Otherwise pop the current tree and repeat.
            self.encoded_offset += encode::encoded_subtree_size(current_subtree.len());
            self.stack.pop();
        }

        // If we made it out the main loop, we're at EOF.
        (self.encoded_offset, StateNext::Done)
    }

    pub fn feed_header(&mut self, header: [u8; HEADER_SIZE]) {
        assert!(self.content_length.is_none(), "second call to feed_header");
        let content_length = hash::decode_len(header);
        self.content_length = Some(content_length);
        self.reset_to_root();
    }

    pub fn feed_parent(&mut self, parent: hash::ParentNode) -> std::result::Result<(), ()> {
        let content_length = self.content_length.expect("feed_parent before header");
        let current_subtree = *self.stack.last().expect("feed_parent after EOF");
        if current_subtree.len() <= CHUNK_SIZE as u64 {
            panic!("too many calls to feed_parent");
        }
        let computed_hash = hash::hash_node(&parent, current_subtree.finalization(content_length));
        if !constant_time_eq(&current_subtree.hash, &computed_hash) {
            return Err(());
        }
        let split = current_subtree.start + hash::left_len(current_subtree.len());
        let left_subtree = Subtree {
            hash: *array_ref!(parent, 0, HASH_SIZE),
            start: current_subtree.start,
            end: split,
        };
        let right_subtree = Subtree {
            hash: *array_ref!(parent, HASH_SIZE, HASH_SIZE),
            start: split,
            end: current_subtree.end,
        };
        self.stack.pop();
        self.stack.push(right_subtree);
        self.stack.push(left_subtree);
        self.encoded_offset += PARENT_SIZE as u128;
        self.length_verified = true;
        Ok(())
    }

    pub fn feed_subtree(&mut self, subtree: Hash) -> std::result::Result<(), ()> {
        let current_subtree = *self.stack.last().expect("feed_subtree after EOF");
        if !constant_time_eq(&subtree, &current_subtree.hash) {
            return Err(());
        }
        self.stack.pop();
        self.content_position = current_subtree.end;
        self.encoded_offset += encode::encoded_subtree_size(current_subtree.len());
        self.length_verified = true;
        Ok(())
    }
}

#[derive(Clone, Copy, Debug)]
pub enum StateNext {
    Header,
    Subtree {
        size: u64,
        skip: u64,
        finalization: Finalization,
    },
    Chunk {
        size: usize,
        skip: usize,
        finalization: Finalization,
    },
    Done,
}

// TODO: Abolish this type!
#[derive(Copy, Clone, Debug)]
struct Subtree {
    hash: Hash,
    start: u64,
    end: u64,
}

impl Subtree {
    fn len(&self) -> u64 {
        self.end - self.start
    }

    fn is_root(&self, content_length: u64) -> bool {
        self.start == 0 && self.end == content_length
    }

    fn finalization(&self, content_length: u64) -> Finalization {
        if self.is_root(content_length) {
            Root(self.len())
        } else {
            NotRoot
        }
    }

    fn state_next(&self, content_length: u64, content_position: u64) -> StateNext {
        let skip = content_position - self.start;
        if self.len() <= CHUNK_SIZE as u64 {
            StateNext::Chunk {
                size: self.len() as usize,
                skip: skip as usize,
                finalization: self.finalization(content_length),
            }
        } else {
            StateNext::Subtree {
                size: self.len(),
                skip,
                finalization: self.finalization(content_length),
            }
        }
    }
}

pub struct Reader<T: Read> {
    inner: T,
    state: State,
    buf: [u8; CHUNK_SIZE],
    buf_start: usize,
    buf_end: usize,
}

impl<T: Read> Reader<T> {
    pub fn new(inner: T, root_hash: Hash) -> Self {
        Self {
            inner,
            state: State::new(root_hash),
            buf: [0; CHUNK_SIZE],
            buf_start: 0,
            buf_end: 0,
        }
    }

    fn buf_len(&self) -> usize {
        self.buf_end - self.buf_start
    }

    fn read_header(&mut self) -> io::Result<()> {
        let mut header = [0; HEADER_SIZE];
        self.inner.read_exact(&mut header)?;
        self.state.feed_header(header);
        Ok(())
    }

    fn read_parent(&mut self) -> io::Result<()> {
        let mut parent = [0; PARENT_SIZE];
        self.inner.read_exact(&mut parent)?;
        into_io(self.state.feed_parent(parent))
    }

    fn read_chunk(
        &mut self,
        size: usize,
        skip: usize,
        finalization: Finalization,
    ) -> io::Result<()> {
        // Clear the buffer before doing any IO, so that in case of failure subsequent reads don't
        // think there's valid data in the buffer.
        self.buf_start = 0;
        self.buf_end = 0;
        self.inner.read_exact(&mut self.buf[..size])?;
        let hash = hash::hash_node(&self.buf[..size], finalization);
        into_io(self.state.feed_subtree(hash))?;
        self.buf_start = skip;
        self.buf_end = size;
        Ok(())
    }
}

impl<T: Read> Read for Reader<T> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.buf_len() == 0 {
            loop {
                match self.state.read_next() {
                    StateNext::Header => self.read_header()?,
                    StateNext::Subtree { .. } => self.read_parent()?,
                    StateNext::Chunk {
                        size,
                        skip,
                        finalization,
                    } => {
                        self.read_chunk(size, skip, finalization)?;
                    }
                    StateNext::Done => return Ok(0), // EOF
                }
            }
        }
        let take = cmp::min(self.buf_len(), buf.len());
        buf[..take].copy_from_slice(&self.buf[self.buf_start..self.buf_start + take]);
        self.buf_start += take;
        Ok(take)
    }
}

impl<T: Read + Seek> Seek for Reader<T> {
    fn seek(&mut self, pos: io::SeekFrom) -> io::Result<u64> {
        // First, read and verify the length if we haven't already.
        let content_length = loop {
            match self.state.len_next() {
                Left(len) => break len,
                Right(StateNext::Header) => self.read_header()?,
                Right(StateNext::Subtree { .. }) => self.read_parent()?,
                Right(StateNext::Chunk {
                    size,
                    skip,
                    finalization,
                }) => self.read_chunk(size, skip, finalization)?,
                Right(StateNext::Done) => unreachable!(),
            }
        };

        // Then, compute the absolute position of the seek.
        let position = match pos {
            io::SeekFrom::Start(pos) => pos,
            io::SeekFrom::End(off) => add_offset(content_length, off)?,
            io::SeekFrom::Current(off) => add_offset(self.state.position(), off)?,
        };

        // Finally, loop over the seek_next() method until it's done.
        loop {
            let (seek_offset, next) = self.state.seek_next(position);
            let cast_offset = cast_offset(seek_offset)?;
            self.inner.seek(io::SeekFrom::Start(cast_offset))?;
            match next {
                StateNext::Header => {
                    self.read_header()?;
                }
                StateNext::Subtree { .. } => {
                    self.read_parent()?;
                }
                StateNext::Chunk {
                    size,
                    skip,
                    finalization,
                } => {
                    self.read_chunk(size, skip, finalization)?;
                }
                StateNext::Done => return Ok(self.state.position()),
            }
        }
    }
}

fn into_io<T>(r: std::result::Result<T, ()>) -> io::Result<T> {
    r.map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "hash mismatch"))
}

fn cast_offset(offset: u128) -> io::Result<u64> {
    if offset > u64::max_value() as u128 {
        Err(io::Error::new(
            io::ErrorKind::Other,
            "seek offset overflowed u64",
        ))
    } else {
        Ok(offset as u64)
    }
}

fn add_offset(position: u64, offset: i64) -> io::Result<u64> {
    let sum = position as i128 + offset as i128;
    if sum < 0 {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "seek before beginning",
        ))
    } else if sum > u64::max_value() as i128 {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "seek target overflowed u64",
        ))
    } else {
        Ok(sum as u64)
    }
}
