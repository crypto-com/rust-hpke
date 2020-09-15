use crate::{
    kdf::{Kdf as KdfTrait, LabeledExpand},
    kem::Kem as KemTrait,
    kex::{Deserializable, Serializable},
    setup::ExporterSecret,
    util::{full_suite_id, FullSuiteId},
    HpkeError,
};

use core::{marker::PhantomData, u8};

use aead::{AeadInPlace as BaseAead, NewAead as BaseNewAead};
use byteorder::{BigEndian, ByteOrder};
use generic_array::GenericArray;
use hkdf::Hkdf;

/// Represents authenticated encryption functionality
pub trait Aead {
    /// The underlying AEAD implementation
    type AeadImpl: BaseAead + BaseNewAead + Clone;

    /// The algorithm identifier for an AEAD implementation
    const AEAD_ID: u16;
}

/// The implementation of AES-GCM-128
pub struct AesGcm128 {}

impl Aead for AesGcm128 {
    type AeadImpl = aes_gcm::Aes128Gcm;

    // draft02 §8.3: AES-GCM-128
    const AEAD_ID: u16 = 0x0001;
}

/// The implementation of AES-GCM-128
pub struct AesGcm256 {}

impl Aead for AesGcm256 {
    type AeadImpl = aes_gcm::Aes256Gcm;

    // draft02 §8.3: AES-GCM-256
    const AEAD_ID: u16 = 0x0002;
}

/// The implementation of ChaCha20-Poly1305
pub struct ChaCha20Poly1305 {}

impl Aead for ChaCha20Poly1305 {
    type AeadImpl = chacha20poly1305::ChaCha20Poly1305;

    // draft02 §8.3: ChaCha20Poly1305
    const AEAD_ID: u16 = 0x0003;
}

// A nonce is the same thing as a sequence counter. But you never increment a nonce.
pub(crate) type AeadNonce<A> = GenericArray<u8, <<A as Aead>::AeadImpl as BaseAead>::NonceSize>;
pub(crate) type AeadKey<A> = GenericArray<u8, <<A as Aead>::AeadImpl as aead::NewAead>::KeySize>;

/// A sequence counter. This is set to `u64` instead of the true nonce size of an AEAD for two
/// reasons:
///
/// 1. No algorithm that would appear in HPKE would support nonce sizes less than `u64`.
/// 2. It is just about physically impossible to encrypt 2^64 messages in sequence. If a computer
///    computes 1 encryption every nanosecond, it would take over 584 years to run out of nonces.
///    Notably, unlike randomized nonces, counting in sequence doesn't parallelize, so we don't
///    have to imagine amortizing this computation across multiple computers. In conclusion, 64
///    bits should be enough for anybody.
#[derive(Default, Clone)]
struct Seq(u64);

// def Context.IncrementSeq():
//   if self.seq >= (1 << (8*Nn)) - 1:
//     raise NonceOverflowError
//   self.seq += 1
/// Increments the sequence counter. Returns None on overflow.
fn increment_seq(seq: &Seq) -> Option<Seq> {
    // Try to add 1
    seq.0.checked_add(1).map(Seq)
}

// def Context.ComputeNonce(seq):
//   seq_bytes = I2OSP(seq, Nn)
//   return xor(self.nonce, seq_bytes)
/// Derives a nonce from the given nonce and a "sequence number". The sequence number is treated as
/// a big-endian integer with length equal to the nonce length.
fn mix_nonce<A: Aead>(base_nonce: &AeadNonce<A>, seq: &Seq) -> AeadNonce<A> {
    // Write `seq` in big-endian order into a byte buffer that's the size of a nonce
    let mut seq_buf = AeadNonce::<A>::default();
    // We just write to the last seq_size bytes. This is necessary because our AEAD nonces (>= 96
    // bits) are always bigger than the sequence buffer (64 bits). We write to the last 64 bits
    // because this is a big-endian number.
    let seq_size = core::mem::size_of::<Seq>();
    let nonce_size = base_nonce.len();
    BigEndian::write_u64(&mut seq_buf[nonce_size - seq_size..], seq.0);

    // XOR the base nonce bytes with the sequence bytes
    let new_nonce_iter = base_nonce
        .iter()
        .zip(seq_buf.iter())
        .map(|(nonce_byte, seq_byte)| nonce_byte ^ seq_byte);

    // This cannot fail, as the length of Nonce<A> is precisely the length of Seq
    GenericArray::from_exact_iter(new_nonce_iter).unwrap()
}

/// An authenticated encryption tag
pub struct AeadTag<A: Aead>(GenericArray<u8, <A::AeadImpl as BaseAead>::TagSize>);

impl<A: Aead> Serializable for AeadTag<A> {
    type OutputSize = <A::AeadImpl as BaseAead>::TagSize;

    fn to_bytes(&self) -> GenericArray<u8, Self::OutputSize> {
        self.0.clone()
    }
}

impl<A: Aead> Deserializable for AeadTag<A> {
    fn from_bytes(encoded: &[u8]) -> Result<Self, HpkeError> {
        if encoded.len() != Self::size() {
            Err(HpkeError::InvalidEncoding)
        } else {
            // Copy to a fixed-size array
            let mut arr = <GenericArray<u8, Self::OutputSize> as Default>::default();
            arr.copy_from_slice(encoded);
            Ok(AeadTag(arr))
        }
    }
}

/// The HPKE encryption context. This is what you use to `seal` plaintexts and `open` ciphertexts.
pub(crate) struct AeadCtx<A: Aead, Kdf: KdfTrait, Kem: KemTrait> {
    /// Records whether the nonce sequence counter has overflowed
    overflowed: bool,
    /// The underlying AEAD instance. This also does decryption.
    encryptor: A::AeadImpl,
    /// The base nonce which we XOR with sequence numbers
    nonce: AeadNonce<A>,
    /// The exporter secret, used in the `export()` method
    exporter_secret: ExporterSecret<Kdf>,
    /// The running sequence number
    seq: Seq,
    /// This binds the `AeadCtx` to the KEM that made it. Used to generate `suite_id`.
    src_kem: PhantomData<Kem>,
    /// The full ID of the ciphersuite that created this `AeadCtx`. Used for context binding.
    suite_id: FullSuiteId,
}

// Necessary for test_setup_soundness
#[cfg(test)]
impl<A: Aead, Kdf: KdfTrait, Kem: KemTrait> Clone for AeadCtx<A, Kdf, Kem> {
    fn clone(&self) -> AeadCtx<A, Kdf, Kem> {
        AeadCtx {
            overflowed: self.overflowed,
            encryptor: self.encryptor.clone(),
            nonce: self.nonce.clone(),
            exporter_secret: self.exporter_secret.clone(),
            seq: self.seq.clone(),
            src_kem: PhantomData,
            suite_id: self.suite_id.clone(),
        }
    }
}

impl<A: Aead, Kdf: KdfTrait, Kem: KemTrait> AeadCtx<A, Kdf, Kem> {
    /// Makes an AeadCtx from a raw key and nonce
    pub(crate) fn new(
        key: &AeadKey<A>,
        nonce: AeadNonce<A>,
        exporter_secret: ExporterSecret<Kdf>,
    ) -> AeadCtx<A, Kdf, Kem> {
        let suite_id = full_suite_id::<A, Kdf, Kem>();
        AeadCtx {
            overflowed: false,
            encryptor: <A::AeadImpl as aead::NewAead>::new(key),
            nonce,
            exporter_secret,
            seq: <Seq as Default>::default(),
            src_kem: PhantomData,
            suite_id,
        }
    }

    // def Context.Export(exporter_context, L):
    //   return LabeledExpand(self.exporter_secret, "sec", exporter_context, L)
    /// Fills a given buffer with secret bytes derived from this encryption context. This value
    /// does not depend on sequence number, so it is constant for the lifetime of this context.
    pub fn export(&self, exporter_ctx: &[u8], out_buf: &mut [u8]) -> Result<(), HpkeError> {
        // Use our exporter secret as the PRK for an HKDF-Expand op. The only time this fails is
        // when the length of the PRK is not the the underlying hash function's digest size. But
        // that's guaranteed by the type system, so we can unwrap().
        let hkdf_ctx = Hkdf::<Kdf::HashImpl>::from_prk(self.exporter_secret.as_slice()).unwrap();

        // This call either succeeds or returns hkdf::InvalidLength (iff the buffer length is more
        // than 255x the digest size of the underlying hash function)
        hkdf_ctx
            .labeled_expand(&self.suite_id, b"sec", exporter_ctx, out_buf)
            .map_err(|_| HpkeError::InvalidKdfLength)
    }
}

/// The HPKE receiver's context. This is what you use to `open` ciphertexts.
pub struct AeadCtxR<A: Aead, Kdf: KdfTrait, Kem: KemTrait>(AeadCtx<A, Kdf, Kem>);

// AeadCtx -> AeadCtxR via wrapping
impl<A: Aead, Kdf: KdfTrait, Kem: KemTrait> From<AeadCtx<A, Kdf, Kem>> for AeadCtxR<A, Kdf, Kem> {
    fn from(ctx: AeadCtx<A, Kdf, Kem>) -> AeadCtxR<A, Kdf, Kem> {
        AeadCtxR(ctx)
    }
}

// Necessary for test_setup_soundness
#[cfg(test)]
impl<A: Aead, Kdf: KdfTrait, Kem: KemTrait> Clone for AeadCtxR<A, Kdf, Kem> {
    fn clone(&self) -> AeadCtxR<A, Kdf, Kem> {
        self.0.clone().into()
    }
}

impl<A: Aead, Kdf: KdfTrait, Kem: KemTrait> AeadCtxR<A, Kdf, Kem> {
    // def Context.Open(aad, ct):
    //   pt = Open(self.key, self.ComputeNonce(self.seq), aad, ct)
    //   if pt == OpenError:
    //     raise OpenError
    //   self.IncrementSeq()
    //   return pt
    /// Does a "detached open in place", meaning it overwrites `ciphertext` with the resulting
    /// plaintext, and takes the tag as a separate input.
    ///
    /// Return Value
    /// ============
    /// Returns `Ok(())` on success.  If this context has been used for so many encryptions that
    /// the sequence number overflowed, returns `Err(HpkeError::SeqOverflow)`. If this happens,
    /// `plaintext` will be unmodified. If the tag fails to validate, returns
    /// `Err(HpkeError::InvalidTag)`. If this happens, `plaintext` is in an undefined state.
    pub fn open(
        &mut self,
        ciphertext: &mut [u8],
        aad: &[u8],
        tag: &AeadTag<A>,
    ) -> Result<(), HpkeError> {
        if self.0.overflowed {
            // If the sequence counter overflowed, we've been used for far too long. Shut down.
            Err(HpkeError::SeqOverflow)
        } else {
            // Compute the nonce and do the encryption in place
            let nonce = mix_nonce::<A>(&self.0.nonce, &self.0.seq);
            let decrypt_res = self
                .0
                .encryptor
                .decrypt_in_place_detached(&nonce, &aad, ciphertext, &tag.0);

            if decrypt_res.is_err() {
                // Opening failed due to a bad tag
                return Err(HpkeError::InvalidTag);
            }

            // Opening was a success
            // Try to increment the sequence counter. If it fails, this was our last
            // decryption.
            match increment_seq(&self.0.seq) {
                Some(new_seq) => self.0.seq = new_seq,
                None => self.0.overflowed = true,
            }

            Ok(())
        }
    }

    /// Fills a given buffer with secret bytes derived from this encryption context. This value
    /// does not depend on sequence number, so it is constant for the lifetime of this context.
    ///
    /// Return Value
    /// ============
    /// Returns `Ok(())` on success. If the buffer length is more than about 255x the digest size
    /// of the underlying hash function, returns an `Err(HpkeError::InvalidKdfLength)`. The exact
    /// number is given in the "Input Length Restrictions" section of the spec. Just don't use to
    /// fill massive buffers and you'll be fine.
    pub fn export(&self, info: &[u8], out_buf: &mut [u8]) -> Result<(), HpkeError> {
        // Pass to AeadCtx
        self.0.export(info, out_buf)
    }
}

/// The HPKE senders's context. This is what you use to `seal` plaintexts.
pub struct AeadCtxS<A: Aead, Kdf: KdfTrait, Kem: KemTrait>(AeadCtx<A, Kdf, Kem>);

// AeadCtx -> AeadCtxS via wrapping
impl<A: Aead, Kdf: KdfTrait, Kem: KemTrait> From<AeadCtx<A, Kdf, Kem>> for AeadCtxS<A, Kdf, Kem> {
    fn from(ctx: AeadCtx<A, Kdf, Kem>) -> AeadCtxS<A, Kdf, Kem> {
        AeadCtxS(ctx)
    }
}

// Necessary for test_setup_soundness
#[cfg(test)]
impl<A: Aead, Kdf: KdfTrait, Kem: KemTrait> Clone for AeadCtxS<A, Kdf, Kem> {
    fn clone(&self) -> AeadCtxS<A, Kdf, Kem> {
        self.0.clone().into()
    }
}

impl<A: Aead, Kdf: KdfTrait, Kem: KemTrait> AeadCtxS<A, Kdf, Kem> {
    // def Context.Seal(aad, pt):
    //   ct = Seal(self.key, self.ComputeNonce(self.seq), aad, pt)
    //   self.IncrementSeq()
    //   return ct
    /// Does a "detached seal in place", meaning it overwrites `plaintext` with the resulting
    /// ciphertext, and returns the resulting authentication tag
    ///
    /// Return Value
    /// ============
    /// Returns `Ok(tag)` on success.  If this context has been used for so many encryptions that
    /// the sequence number overflowed, returns `Err(HpkeError::SeqOverflow)`. If this happens,
    /// `plaintext` will be unmodified. If an unspecified error happened during encryption, returns
    /// `Err(HpkeError::Encryption)`. If this happens, the contents of `plaintext` is undefined.
    pub fn seal(&mut self, plaintext: &mut [u8], aad: &[u8]) -> Result<AeadTag<A>, HpkeError> {
        if self.0.overflowed {
            // If the sequence counter overflowed, we've been used for far too long. Shut down.
            Err(HpkeError::SeqOverflow)
        } else {
            // Compute the nonce and do the encryption in place
            let nonce = mix_nonce::<A>(&self.0.nonce, &self.0.seq);
            let tag_res = self
                .0
                .encryptor
                .encrypt_in_place_detached(&nonce, &aad, plaintext);

            // Check if an error occurred when encrypting
            let tag = match tag_res {
                Err(_) => return Err(HpkeError::Encryption),
                Ok(t) => t,
            };

            // Try to increment the sequence counter. If it fails, this was our last encryption.
            match increment_seq(&self.0.seq) {
                Some(new_seq) => self.0.seq = new_seq,
                None => self.0.overflowed = true,
            }

            // Return the tag
            Ok(AeadTag(tag))
        }
    }

    // def Context.Export(exporter_context, L):
    //   return LabeledExpand(self.exporter_secret, "sec", exporter_context, L)
    /// Fills a given buffer with secret bytes derived from this encryption context. This value
    /// does not depend on sequence number, so it is constant for the lifetime of this context.
    ///
    /// Return Value
    /// ============
    /// Returns `Ok(())` on success. If the buffer length is more than 255x the digest size of the
    /// underlying hash function, returns an `Err(HpkeError::InvalidKdfLength)`.
    pub fn export(&self, info: &[u8], out_buf: &mut [u8]) -> Result<(), HpkeError> {
        // Pass to AeadCtx
        self.0.export(info, out_buf)
    }
}

#[cfg(test)]
mod test {
    use super::{AeadTag, AesGcm128, AesGcm256, ChaCha20Poly1305, Seq};
    use crate::{kdf::HkdfSha256, kex::Deserializable, test_util::gen_ctx_simple_pair, HpkeError};

    /// Tests that encryption context secret export does not change behavior based on the
    /// underlying sequence number This logic is cipher-agnostic, so we don't make the test generic
    /// over ciphers.
    macro_rules! test_export_idempotence {
        ($test_name:ident, $kem_ty:ty) => {
            #[test]
            fn $test_name() {
                type Kem = $kem_ty;
                type Kdf = HkdfSha256;
                // Again, this test is cipher-agnostic
                type A = ChaCha20Poly1305;

                // Set up a context. Logic is algorithm-independent, so we don't care about the
                // types here
                let (mut sender_ctx, _) = gen_ctx_simple_pair::<A, Kdf, Kem>();

                // Get an initial export secret
                let mut secret1 = [0u8; 16];
                sender_ctx
                    .export(b"test_export_idempotence", &mut secret1)
                    .unwrap();

                // Modify the context by encrypting something
                let mut plaintext = *b"back hand";
                sender_ctx
                    .seal(&mut plaintext[..], b"")
                    .expect("seal() failed");

                // Get a second export secret
                let mut secret2 = [0u8; 16];
                sender_ctx
                    .export(b"test_export_idempotence", &mut secret2)
                    .unwrap();

                assert_eq!(secret1, secret2);
            }
        };
    }

    /// Tests that sequence overflowing causes an error. This logic is cipher-agnostic, so we don't
    /// make the test generic over ciphers.
    macro_rules! test_overflow {
        ($test_name:ident, $kem_ty:ty) => {
            #[test]
            fn $test_name() {
                type Kem = $kem_ty;
                type Kdf = HkdfSha256;
                // Again, this test is cipher-agnostic
                type A = ChaCha20Poly1305;

                // Make a sequence number that's at the max
                let big_seq = {
                    let mut seq = <Seq as Default>::default();
                    seq.0 = u64::MAX;
                    seq
                };

                let (mut sender_ctx, mut receiver_ctx) = gen_ctx_simple_pair::<A, Kdf, Kem>();
                sender_ctx.0.seq = big_seq.clone();
                receiver_ctx.0.seq = big_seq.clone();

                // These should support precisely one more encryption before it registers an
                // overflow

                let msg = b"draxx them sklounst";
                let aad = b"with my prayers";

                // Do one round trip and ensure it works
                {
                    let mut plaintext = *msg;
                    // Encrypt the plaintext
                    let tag = sender_ctx
                        .seal(&mut plaintext[..], aad)
                        .expect("seal() failed");
                    // Rename for clarity
                    let mut ciphertext = plaintext;

                    // Now to decrypt on the other side
                    receiver_ctx
                        .open(&mut ciphertext[..], aad, &tag)
                        .expect("open() failed");
                    // Rename for clarity
                    let roundtrip_plaintext = ciphertext;

                    // Make sure the output message was the same as the input message
                    assert_eq!(msg, &roundtrip_plaintext);
                }

                // Try another round trip and ensure that we've overflowed
                {
                    let mut plaintext = *msg;
                    // Try to encrypt the plaintext
                    match sender_ctx.seal(&mut plaintext[..], aad) {
                        Err(HpkeError::SeqOverflow) => {} // Good, this should have overflowed
                        Err(e) => panic!("seal() should have overflowed. Instead got {}", e),
                        _ => panic!("seal() should have overflowed. Instead it succeeded"),
                    }

                    // Now try to decrypt something. This isn't a valid ciphertext or tag, but the
                    // overflow should fail before the tag check fails.
                    let mut dummy_ciphertext = [0u8; 32];
                    let dummy_tag = AeadTag::from_bytes(&[0; 16]).unwrap();

                    match receiver_ctx.open(&mut dummy_ciphertext[..], aad, &dummy_tag) {
                        Err(HpkeError::SeqOverflow) => {} // Good, this should have overflowed
                        Err(e) => panic!("open() should have overflowed. Instead got {}", e),
                        _ => panic!("open() should have overflowed. Instead it succeeded"),
                    }
                }
            }
        };
    }

    /// Tests that `open()` can decrypt things properly encrypted with `seal()`
    macro_rules! test_ctx_correctness {
        ($test_name:ident, $aead_ty:ty, $kem_ty:ty) => {
            #[test]
            fn $test_name() {
                type A = $aead_ty;
                type Kdf = HkdfSha256;
                type Kem = $kem_ty;

                let (mut sender_ctx, mut receiver_ctx) = gen_ctx_simple_pair::<A, Kdf, Kem>();

                let msg = b"Love it or leave it, you better gain way";
                let aad = b"You better hit bull's eye, the kid don't play";

                // Encrypt with the sender context
                let mut ciphertext = msg.clone();
                let tag = sender_ctx
                    .seal(&mut ciphertext[..], aad)
                    .expect("seal() failed");

                // Make sure seal() isn't a no-op
                assert!(&ciphertext[..] != &msg[..]);

                // Decrypt with the receiver context
                receiver_ctx
                    .open(&mut ciphertext[..], aad, &tag)
                    .expect("open() failed");
                // Change name for clarity
                let decrypted = ciphertext;
                assert_eq!(&decrypted[..], &msg[..]);
            }
        };
    }

    #[cfg(feature = "x25519-dalek")]
    test_export_idempotence!(test_export_idempotence_x25519, crate::kem::X25519HkdfSha256);
    #[cfg(feature = "p256")]
    test_export_idempotence!(test_export_idempotence_p256, crate::kem::DhP256HkdfSha256);

    #[cfg(feature = "x25519-dalek")]
    test_overflow!(test_overflow_x25519, crate::kem::X25519HkdfSha256);
    #[cfg(feature = "p256")]
    test_overflow!(test_overflow_p256, crate::kem::DhP256HkdfSha256);

    #[cfg(feature = "x25519-dalek")]
    test_ctx_correctness!(
        test_ctx_correctness_aes128_x25519,
        AesGcm128,
        crate::kem::X25519HkdfSha256
    );
    #[cfg(feature = "p256")]
    test_ctx_correctness!(
        test_ctx_correctness_aes128_p256,
        AesGcm128,
        crate::kem::DhP256HkdfSha256
    );
    #[cfg(feature = "x25519-dalek")]
    test_ctx_correctness!(
        test_ctx_correctness_aes256_x25519,
        AesGcm256,
        crate::kem::X25519HkdfSha256
    );
    #[cfg(feature = "p256")]
    test_ctx_correctness!(
        test_ctx_correctness_aes256_p256,
        AesGcm256,
        crate::kem::DhP256HkdfSha256
    );
    #[cfg(feature = "x25519-dalek")]
    test_ctx_correctness!(
        test_ctx_correctness_chacha_x25519,
        ChaCha20Poly1305,
        crate::kem::X25519HkdfSha256
    );
    #[cfg(feature = "p256")]
    test_ctx_correctness!(
        test_ctx_correctness_chacha_p256,
        ChaCha20Poly1305,
        crate::kem::DhP256HkdfSha256
    );
}
