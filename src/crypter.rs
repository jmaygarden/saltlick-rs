// Copyright (c) 2019, Nick Stevens <nick@bitcurry.com>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/license/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Low-level API for push/pull implementations of crypto format.
//!
//! # Example
//!
//! ```
//! use saltlick::crypter::{Buf, Decrypter, Encrypter, FromBuf};
//!
//! let test_data = vec![vec![1, 2, 3], vec![4, 5, 6]];
//!
//! let (public, secret) = saltlick::gen_keypair();
//!
//! // Data is pushed into the crypter. Block sizes are handled automatically.
//! let mut encrypter = Encrypter::new(public.clone());
//! let mut ciphertext = Vec::new();
//! for block in test_data.iter() {
//!     ciphertext.extend(encrypter.push(block, false).unwrap().iter())
//! }
//!
//! // Once all data is written, the crypter must be manually finalized. After
//! // this trying to add more data will result in an error. If the stream is not
//! // finalized, decryption will fail as incomplete.
//! ciphertext.extend(encrypter.push(&[] as &[u8], true).unwrap().iter());
//!
//! // Decryption is the opposite of encrypting - feed chunks of ciphertext to
//! // the `Decrypter::pull` function until `Decrypter::is_finalized` returns
//! // true.
//! let mut decrypter = Decrypter::new(public, secret);
//! let plaintext = decrypter.pull(ciphertext).unwrap();
//! assert_eq!(
//!     test_data.into_iter().flatten().collect::<Vec<u8>>(),
//!     Vec::from_buf(plaintext)
//! );
//! assert!(decrypter.is_finalized());
//! ```

use std::cmp;
use std::fmt;
use std::mem;

use byteorder::{ByteOrder, NetworkEndian};
use bytes::BytesMut;
use sodiumoxide::crypto::secretstream::{self, Header, Key, Pull, Push, Stream, Tag};

use crate::error::SaltlickError;
use crate::key::{PublicKey, SecretKey};
use crate::version::Version;

use self::read::ReadStatus;

pub use crate::multibuf::MultiBuf;
pub use bytes::buf::FromBuf;
pub use bytes::{Buf, Bytes, IntoBuf};

/// Minimum block size allowed - values smaller than this will automatically be
/// coerced up to this value.
pub const MIN_BLOCK_SIZE: usize = 1024;

/// Maximum block size allowed - values larger than this will automatically be
/// coerced down to this value.
pub const MAX_BLOCK_SIZE: usize = 8 * 1024 * 1024;

/// Default block size.
pub const DEFAULT_BLOCK_SIZE: usize = 512 * 1024;

const MAGIC: &[u8] = b"SALTLICK";
const MAGIC_LEN: usize = 8;
const MESSAGE_LEN_LEN: usize = secretstream::ABYTES + mem::size_of::<u32>();

enum EncrypterState {
    Start,
    NextBlock(Stream<Push>),
    WriteBlock(Stream<Push>),
    Finalize(Stream<Push>),
    Finalized,
    Error(SaltlickError),
    None,
}

impl fmt::Debug for EncrypterState {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        use self::EncrypterState::*;
        match self {
            Start => write!(f, "EncrypterState::Start"),
            NextBlock(_) => write!(f, "EncrypterState::NextBlock"),
            WriteBlock(_) => write!(f, "EncrypterState::WritBlock"),
            Finalize(_) => write!(f, "EncrypterState::Finalize"),
            Finalized => write!(f, "EncrypterState::Finalized"),
            Error(e) => write!(f, "EncrypterState::Error({:?})", e),
            None => write!(f, "EncrypterState::None"),
        }
    }
}

/// Low-level interface to encrypting data in the saltlick format.
#[derive(Debug)]
pub struct Encrypter {
    block_size: usize,
    plaintext: BytesMut,
    public_key: PublicKey,
    state: EncrypterState,
}

impl Encrypter {
    /// Create a new encrypter using the provided public key.
    pub fn new(public_key: PublicKey) -> Encrypter {
        let mut encrypter = Encrypter {
            block_size: 0,
            plaintext: BytesMut::new(),
            public_key,
            state: EncrypterState::Start,
        };
        encrypter.set_block_size(DEFAULT_BLOCK_SIZE);
        encrypter
    }

    /// Set block size for encrypter.
    pub fn set_block_size(&mut self, block_size: usize) {
        let block_size = cmp::max(MIN_BLOCK_SIZE, cmp::min(block_size, MAX_BLOCK_SIZE));
        self.block_size = block_size;
    }

    /// Push plaintext to the encrypter, receiving encrypted ciphertext in
    /// return.
    pub fn push(
        &mut self,
        plaintext: impl AsRef<[u8]>,
        finalize: bool,
    ) -> Result<MultiBuf, SaltlickError> {
        use self::EncrypterState::*;
        let mut output = MultiBuf::new();
        self.plaintext.extend_from_slice(plaintext.as_ref());
        loop {
            match mem::replace(&mut self.state, None) {
                Start => {
                    let (stream, header) = self.start()?;
                    self.state = NextBlock(stream);
                    output.push(header);
                }
                NextBlock(stream) => {
                    if self.plaintext.len() >= self.block_size {
                        self.state = WriteBlock(stream);
                    } else if finalize {
                        self.state = Finalize(stream);
                    } else {
                        self.state = NextBlock(stream);
                        return Ok(output);
                    }
                }
                WriteBlock(mut stream) => match self.write_block(&mut stream, false) {
                    Ok(buf) => {
                        output.extend(buf);
                        self.state = NextBlock(stream);
                    }
                    Err(e) => {
                        self.state = Error(e);
                    }
                },
                Finalize(mut stream) => match self.write_block(&mut stream, true) {
                    Ok(buf) => {
                        output.extend(buf);
                        self.state = Finalized;
                        return Ok(output);
                    }
                    Err(e) => {
                        self.state = Error(e);
                    }
                },
                Finalized => {
                    self.state = Finalized;
                    return Err(SaltlickError::Finalized);
                }
                Error(e) => {
                    return Err(e);
                }
                None => panic!("Encrypter state machine reached None state"),
            }
        }
    }

    /// Returns true if the crypter has been finalized.
    pub fn is_finalized(&self) -> bool {
        match self.state {
            EncrypterState::Finalized => true,
            _ => false,
        }
    }

    /// Returns true if the crypter has not been finalized.
    pub fn is_not_finalized(&self) -> bool {
        !self.is_finalized()
    }

    fn start(&self) -> Result<(Stream<Push>, Vec<u8>), SaltlickError> {
        let key = secretstream::gen_key();
        let (stream, header) =
            Stream::init_push(&key).map_err(|()| SaltlickError::StreamStartFailure)?;
        Ok((stream, write::header_v1(&key, &header, &self.public_key)))
    }

    fn write_block(
        &mut self,
        stream: &mut Stream<Push>,
        finalize: bool,
    ) -> Result<MultiBuf, SaltlickError> {
        let mut output = MultiBuf::new();
        let message = self
            .plaintext
            .split_to(cmp::min(self.plaintext.len(), self.block_size));
        let mut block_size_buf = [0u8; 4];
        NetworkEndian::write_u32(&mut block_size_buf[..], message.len() as u32);
        output.push(
            stream
                .push(&block_size_buf[..], None, Tag::Message)
                .map_err(|()| SaltlickError::Finalized)?,
        );
        let tag = if finalize { Tag::Final } else { Tag::Message };
        output.push(
            stream
                .push(&message[..], None, tag)
                .map_err(|()| SaltlickError::Finalized)?,
        );
        Ok(output)
    }
}

// This is a workaround to allow calling a Box<FnOnce> in Rust versions less
// than 1.35. When the MSRV is 1.35 or greater, this can be removed and
// replaced directly with `Box<dyn FnOnce(...)>`.
//
// Refer to https://github.com/rust-lang/rust/issues/28796 for more info.
trait KeyLookupFn {
    fn call_box(self: Box<Self>, public_key: &PublicKey) -> Option<SecretKey>;
}

impl<T> KeyLookupFn for T
where
    T: FnOnce(&PublicKey) -> Option<SecretKey>,
{
    fn call_box(self: Box<Self>, public_key: &PublicKey) -> Option<SecretKey> {
        (*self)(public_key)
    }
}

enum KeyResolution {
    Available(PublicKey, SecretKey),
    Deferred(Box<dyn KeyLookupFn>),
}

impl fmt::Debug for KeyResolution {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        use self::KeyResolution::*;
        match self {
            Available(_, _) => write!(f, "KeyResolution::Available"),
            Deferred(_) => write!(f, "KeyResolution::Deferred"),
        }
    }
}

enum DecrypterState {
    ReadPreheader(KeyResolution),
    ReadPublicKey(KeyResolution),
    SecretKeyLookup(PublicKey, Box<dyn KeyLookupFn>),
    ReadHeader(PublicKey, PublicKey, SecretKey),
    OpenStream(Key, Header),
    ReadLength(Stream<Pull>),
    ReadBlock(Stream<Pull>, usize),
    FinalBlock,
    Finalized,
    Error(SaltlickError),
    None,
}

impl fmt::Debug for DecrypterState {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        use self::DecrypterState::*;
        match self {
            ReadPreheader(_) => write!(f, "DecrypterState::ReadPreheader"),
            ReadPublicKey(_) => write!(f, "DecrypterState::ReadPublicKey"),
            SecretKeyLookup(_, _) => write!(f, "DecrypterState::SecretKeyLookup"),
            ReadHeader(_, _, _) => write!(f, "DecrypterState::ReadHeader"),
            OpenStream(_, _) => write!(f, "DecrypterState::OpenStream"),
            ReadLength(_) => write!(f, "DecrypterState::ReadLength"),
            ReadBlock(_, _) => write!(f, "DecrypterState::ReadBlock"),
            FinalBlock => write!(f, "DecrypterState::FinalBlock"),
            Finalized => write!(f, "DecrypterState::Finalized"),
            Error(e) => write!(f, "DecrypterState::Error({:?})", e),
            None => write!(f, "DecrypterState::None"),
        }
    }
}

/// Low-level interface to decrypting data in the saltlick format.
#[derive(Debug)]
pub struct Decrypter {
    ciphertext: BytesMut,
    state: DecrypterState,
}

impl Decrypter {
    /// Create a new decrypter using the provided public and secret key.
    pub fn new(public_key: PublicKey, secret_key: SecretKey) -> Decrypter {
        Decrypter {
            ciphertext: BytesMut::new(),
            state: DecrypterState::ReadPreheader(KeyResolution::Available(public_key, secret_key)),
        }
    }

    /// Create a new decrypter that calls `lookup_fn` with the public key
    /// obtained from the stream to obtain a secret key.
    ///
    /// This function allows for delayed lookup of a secret key - for example,
    /// when there are multiple potential keys that could have been used to
    /// encrypt the file. The lookup function should return the secret key
    /// corresponding to the given public key, or `None` if no appropriate key
    /// is available. In this case, the Decrypter will return a
    /// `SaltlickError::SecretKeyNotFound` error from `pull`.
    pub fn new_deferred<F>(lookup_fn: F) -> Decrypter
    where
        F: FnOnce(&PublicKey) -> Option<SecretKey> + 'static,
    {
        Decrypter {
            ciphertext: BytesMut::new(),
            state: DecrypterState::ReadPreheader(KeyResolution::Deferred(Box::new(lookup_fn))),
        }
    }

    /// Pull ciphertext "through" the decrypter, receiving decrypted plaintext
    /// in return.
    pub fn pull(&mut self, ciphertext: impl AsRef<[u8]>) -> Result<MultiBuf, SaltlickError> {
        use self::DecrypterState::*;
        use self::KeyResolution::*;
        let mut output = MultiBuf::new();
        self.ciphertext.extend_from_slice(ciphertext.as_ref());
        loop {
            match mem::replace(&mut self.state, None) {
                ReadPreheader(key_resolution) => match read::preheader(&self.ciphertext) {
                    Ok(ReadStatus::Complete(version, n)) => {
                        self.ciphertext.advance(n);
                        if version != Version::V1 {
                            self.state = Error(SaltlickError::UnsupportedVersion);
                        } else {
                            self.state = ReadPublicKey(key_resolution);
                        }
                    }
                    Ok(ReadStatus::Incomplete(_needed)) => {
                        self.state = ReadPreheader(key_resolution);
                        return Ok(output);
                    }
                    Err(e) => {
                        self.state = Error(e);
                    }
                },
                ReadPublicKey(key_resolution) => {
                    match read::header_v1_public_key(&self.ciphertext) {
                        Ok(ReadStatus::Complete(file_public_key, n)) => {
                            self.ciphertext.advance(n);
                            match key_resolution {
                                Available(public_key, secret_key) => {
                                    self.state =
                                        ReadHeader(file_public_key, public_key, secret_key);
                                }
                                Deferred(lookup_fn) => {
                                    self.state = SecretKeyLookup(file_public_key, lookup_fn);
                                }
                            }
                        }
                        Ok(ReadStatus::Incomplete(_needed)) => {
                            self.state = ReadPublicKey(key_resolution);
                            return Ok(output);
                        }
                        Err(e) => {
                            self.state = Error(e);
                        }
                    }
                }
                SecretKeyLookup(file_public_key, lookup_fn) => {
                    if let Some(secret_key) = lookup_fn.call_box(&file_public_key) {
                        self.state =
                            ReadHeader(file_public_key.clone(), file_public_key, secret_key);
                    } else {
                        self.state = Error(SaltlickError::SecretKeyNotFound);
                    }
                }
                ReadHeader(file_public_key, public_key, secret_key) => {
                    if file_public_key != public_key {
                        return Err(SaltlickError::PublicKeyMismatch);
                    }
                    match read::header_v1_sealed_text(&self.ciphertext, &public_key, &secret_key) {
                        Ok(ReadStatus::Complete((key, header), n)) => {
                            self.ciphertext.advance(n);
                            self.state = OpenStream(key, header);
                        }
                        Ok(ReadStatus::Incomplete(_needed)) => {
                            self.state = ReadHeader(file_public_key, public_key, secret_key);
                            return Ok(output);
                        }
                        Err(e) => {
                            self.state = Error(e);
                        }
                    }
                }
                OpenStream(key, header) => match Stream::init_pull(&header, &key) {
                    Ok(stream) => {
                        self.state = ReadLength(stream);
                    }
                    Err(()) => {
                        self.state = Error(SaltlickError::DecryptionFailure);
                    }
                },
                ReadLength(mut stream) => match read::length(&self.ciphertext, &mut stream) {
                    Ok(ReadStatus::Complete(length, n)) => {
                        self.ciphertext.advance(n);
                        self.state = ReadBlock(stream, length);
                    }
                    Ok(ReadStatus::Incomplete(_needed)) => {
                        self.state = ReadLength(stream);
                        return Ok(output);
                    }
                    Err(e) => {
                        self.state = Error(e);
                    }
                },
                ReadBlock(mut stream, length) => {
                    match read::block(&self.ciphertext, &mut stream, length) {
                        Ok(ReadStatus::Complete((plaintext, finalized), n)) => {
                            self.ciphertext.advance(n);
                            output.push(plaintext);
                            if finalized {
                                self.state = FinalBlock;
                            } else {
                                self.state = ReadLength(stream);
                            }
                        }
                        Ok(ReadStatus::Incomplete(_needed)) => {
                            self.state = ReadBlock(stream, length);
                            return Ok(output);
                        }
                        Err(e) => {
                            self.state = Error(e);
                        }
                    }
                }
                FinalBlock => {
                    self.state = Finalized;
                    return Ok(output);
                }
                Finalized => {
                    self.state = Finalized;
                    return Err(SaltlickError::Finalized);
                }
                Error(e) => {
                    return Err(e);
                }
                None => panic!("Decrypter state machine reached None state"),
            }
        }
    }

    /// Returns true if the crypter has been finalized.
    pub fn is_finalized(&self) -> bool {
        match self.state {
            DecrypterState::Finalized => true,
            _ => false,
        }
    }

    /// Returns true if the crypter has not been finalized.
    pub fn is_not_finalized(&self) -> bool {
        !self.is_finalized()
    }
}

mod read {
    use std::mem;

    use byteorder::{ByteOrder, NetworkEndian};
    use sodiumoxide::crypto::{
        box_::PUBLICKEYBYTES,
        sealedbox::{self, SEALBYTES},
        secretstream::{Header, Key, Pull, Stream, Tag, ABYTES, HEADERBYTES, KEYBYTES},
    };

    use super::{PublicKey, SaltlickError, SecretKey, Version, MAGIC, MAGIC_LEN, MESSAGE_LEN_LEN};

    const PREHEADER_LEN: usize = MAGIC_LEN + mem::size_of::<u8>();
    const SEALEDTEXT_LEN: usize = KEYBYTES + HEADERBYTES + SEALBYTES;

    pub enum ReadStatus<T> {
        Incomplete(usize),
        Complete(T, usize),
    }

    pub fn preheader(input: impl AsRef<[u8]>) -> Result<ReadStatus<Version>, SaltlickError> {
        let input_len = input.as_ref().len();
        if input_len < PREHEADER_LEN {
            return Ok(ReadStatus::Incomplete(PREHEADER_LEN - input_len));
        }
        if &input.as_ref()[..MAGIC.len()] != MAGIC {
            return Err(SaltlickError::BadMagic);
        }
        let version = Version::from_u8(input.as_ref()[MAGIC.len()]);

        Ok(ReadStatus::Complete(version, PREHEADER_LEN))
    }

    pub fn header_v1_public_key(
        input: impl AsRef<[u8]>,
    ) -> Result<ReadStatus<PublicKey>, SaltlickError> {
        let input_len = input.as_ref().len();
        if input_len < PUBLICKEYBYTES {
            return Ok(ReadStatus::Incomplete(PUBLICKEYBYTES - input_len));
        }
        let public_key = PublicKey::from_raw_curve25519(&input.as_ref()[..PUBLICKEYBYTES])?;
        Ok(ReadStatus::Complete(public_key, PUBLICKEYBYTES))
    }

    pub fn header_v1_sealed_text(
        input: impl AsRef<[u8]>,
        public_key: &PublicKey,
        secret_key: &SecretKey,
    ) -> Result<ReadStatus<(Key, Header)>, SaltlickError> {
        let input_len = input.as_ref().len();
        if input_len < SEALEDTEXT_LEN {
            return Ok(ReadStatus::Incomplete(SEALEDTEXT_LEN - input_len));
        }
        let sealed_text = &input.as_ref()[..SEALEDTEXT_LEN];
        let plaintext = sealedbox::open(sealed_text, &public_key.inner, &secret_key.inner)
            .map_err(|()| SaltlickError::DecryptionFailure)?;
        let symmetric_key =
            Key::from_slice(&plaintext[..KEYBYTES]).ok_or(SaltlickError::DecryptionFailure)?;
        let stream_header = Header::from_slice(&plaintext[KEYBYTES..(KEYBYTES + HEADERBYTES)])
            .ok_or(SaltlickError::DecryptionFailure)?;
        Ok(ReadStatus::Complete(
            (symmetric_key, stream_header),
            SEALEDTEXT_LEN,
        ))
    }

    pub fn length(
        input: impl AsRef<[u8]>,
        stream: &mut Stream<Pull>,
    ) -> Result<ReadStatus<usize>, SaltlickError> {
        let input_len = input.as_ref().len();
        if input_len < MESSAGE_LEN_LEN {
            return Ok(ReadStatus::Incomplete(MESSAGE_LEN_LEN - input_len));
        }
        let (plaintext, tag) = stream
            .pull(&input.as_ref()[..MESSAGE_LEN_LEN], None)
            .map_err(|()| SaltlickError::DecryptionFailure)?;
        if tag != Tag::Message {
            // A length block should never be the end of the stream
            return Err(SaltlickError::DecryptionFailure);
        }
        Ok(ReadStatus::Complete(
            NetworkEndian::read_u32(&plaintext) as usize,
            MESSAGE_LEN_LEN,
        ))
    }

    pub fn block(
        input: impl AsRef<[u8]>,
        stream: &mut Stream<Pull>,
        message_length: usize,
    ) -> Result<ReadStatus<(Vec<u8>, bool)>, SaltlickError> {
        let input_len = input.as_ref().len();
        let block_len = message_length + ABYTES;
        if input_len < block_len {
            return Ok(ReadStatus::Incomplete(block_len - input_len));
        }
        let (plaintext, tag) = stream
            .pull(&input.as_ref()[..block_len], None)
            .map_err(|()| SaltlickError::DecryptionFailure)?;
        match tag {
            Tag::Message if message_length == 0 => {
                // The only message allowed to be zero-length is the final
                // message.
                Err(SaltlickError::DecryptionFailure)
            }
            Tag::Message => Ok(ReadStatus::Complete((plaintext, false), block_len)),
            Tag::Final => Ok(ReadStatus::Complete((plaintext, true), block_len)),
            Tag::Push | Tag::Rekey => Err(SaltlickError::DecryptionFailure),
        }
    }
}

mod write {
    use sodiumoxide::crypto::{
        sealedbox,
        secretstream::{Header, Key},
    };

    use super::{PublicKey, Version, MAGIC};

    pub fn preheader(version: Version) -> Vec<u8> {
        let mut header = Vec::from(MAGIC);
        header.push(version.to_u8());
        header
    }

    pub fn header_v1(
        symmetric_key: &Key,
        stream_header: &Header,
        public_key: &PublicKey,
    ) -> Vec<u8> {
        let mut to_encrypt = Vec::new();
        to_encrypt.extend(&symmetric_key[..]);
        to_encrypt.extend(&stream_header[..]);

        let mut header = preheader(Version::V1);
        header.extend(&public_key.inner[..]);
        header.extend(sealedbox::seal(&to_encrypt, &public_key.inner));
        header
    }
}

#[cfg(test)]
mod tests {
    use bytes::buf::FromBuf;
    use bytes::Buf;
    use rand::{RngCore, SeedableRng};
    use rand_xorshift::XorShiftRng;

    use crate::error::SaltlickError;
    use crate::key;

    use super::{Decrypter, Encrypter};

    fn random_bytes(seed: u64, size: usize) -> Vec<u8> {
        let mut rng = XorShiftRng::seed_from_u64(seed);
        let mut bytes = vec![0u8; size];
        rng.fill_bytes(&mut bytes);
        bytes
    }

    #[test]
    fn simple_test() {
        let test_data = vec![
            random_bytes(0, 567),
            random_bytes(1, 1337),
            random_bytes(2, 16742),
        ];
        let (public, secret) = key::gen_keypair();

        let mut encrypter = Encrypter::new(public.clone());
        let mut ciphertext = Vec::new();
        encrypter.set_block_size(1500);
        for block in test_data.iter() {
            ciphertext.extend(encrypter.push(block, false).unwrap().iter())
        }
        ciphertext.extend(encrypter.push(&[] as &[u8], true).unwrap().iter());

        let mut decrypter = Decrypter::new(public, secret);
        let plaintext = decrypter.pull(ciphertext).unwrap();
        assert_eq!(
            test_data.into_iter().flatten().collect::<Vec<u8>>(),
            Vec::from_buf(plaintext)
        );
        assert!(decrypter.is_finalized());
    }

    #[test]
    fn one_byte_at_a_time_test() {
        let test_data = random_bytes(3, 25000);
        let (public, secret) = key::gen_keypair();

        let mut encrypter = Encrypter::new(public.clone());
        let mut ciphertext = Vec::new();
        encrypter.set_block_size(500);
        for byte in test_data.iter() {
            ciphertext.extend(encrypter.push(&[*byte], false).unwrap().iter())
        }
        ciphertext.extend(encrypter.push(&[] as &[u8], true).unwrap().iter());

        let mut decrypter = Decrypter::new(public, secret);
        let mut plaintext = Vec::new();
        for byte in ciphertext {
            plaintext.extend(Vec::from_buf(decrypter.pull(&[byte]).unwrap()));
        }
        assert_eq!(test_data, plaintext);
        assert!(decrypter.is_finalized());
    }

    #[test]
    fn deferred_key_load_test() {
        let test_data = random_bytes(4, 25000);
        let (public, secret) = key::gen_keypair();

        let mut encrypter = Encrypter::new(public.clone());
        let mut ciphertext = Vec::new();
        ciphertext.extend(encrypter.push(&test_data[..], true).unwrap().iter());

        let mut decrypter = Decrypter::new_deferred(move |_public| Some(secret));
        let plaintext = Vec::from_buf(decrypter.pull(&ciphertext[..]).unwrap());

        assert_eq!(test_data, plaintext);
        assert!(decrypter.is_finalized());
    }

    #[test]
    fn deferred_key_load_failure_test() {
        let test_data = random_bytes(5, 25000);
        let (public, _secret) = key::gen_keypair();

        let mut encrypter = Encrypter::new(public.clone());
        let mut ciphertext = Vec::new();
        ciphertext.extend(encrypter.push(&test_data[..], true).unwrap().iter());

        let mut decrypter = Decrypter::new_deferred(move |_public| None);
        assert_eq!(
            SaltlickError::SecretKeyNotFound,
            decrypter.pull(&ciphertext[..]).unwrap_err()
        );
    }
}
