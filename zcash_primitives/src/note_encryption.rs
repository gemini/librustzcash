//! Implementation of in-band secret distribution for Zcash transactions.

use crate::{
    consensus,
    consensus::{NetworkUpgrade, ZIP212_GRACE_PERIOD},
    jubjub::{
        edwards,
        fs::{Fs, FsRepr},
        PrimeOrder, Unknown,
    },
    primitives::{Diversifier, Note, PaymentAddress, Rseed},
};
use blake2b_simd::{Hash as Blake2bHash, Params as Blake2bParams};
use byteorder::{LittleEndian, ReadBytesExt, WriteBytesExt};
use crypto_api_chachapoly::{ChaCha20Ietf, ChachaPolyIetf};
use ff::PrimeField;
use pairing::bls12_381::{Bls12, Fr};
use rand_core::{CryptoRng, RngCore};
use std::convert::TryInto;
use std::fmt;
use std::str;

use crate::{keys::OutgoingViewingKey, JUBJUB};

pub const KDF_SAPLING_PERSONALIZATION: &[u8; 16] = b"Zcash_SaplingKDF";
pub const PRF_OCK_PERSONALIZATION: &[u8; 16] = b"Zcash_Derive_ock";

const COMPACT_NOTE_SIZE: usize = 1 + // version
    11 + // diversifier
    8  + // value
    32; // rcv
const NOTE_PLAINTEXT_SIZE: usize = COMPACT_NOTE_SIZE + 512;
const OUT_PLAINTEXT_SIZE: usize = 32 + // pk_d
    32; // esk
const ENC_CIPHERTEXT_SIZE: usize = NOTE_PLAINTEXT_SIZE + 16;
const OUT_CIPHERTEXT_SIZE: usize = OUT_PLAINTEXT_SIZE + 16;

/// Format a byte array as a colon-delimited hex string.
///
/// Source: https://github.com/tendermint/signatory
/// License: MIT / Apache 2.0
fn fmt_colon_delimited_hex<B>(f: &mut fmt::Formatter<'_>, bytes: B) -> fmt::Result
where
    B: AsRef<[u8]>,
{
    let len = bytes.as_ref().len();

    for (i, byte) in bytes.as_ref().iter().enumerate() {
        write!(f, "{:02x}", byte)?;

        if i != len - 1 {
            write!(f, ":")?;
        }
    }

    Ok(())
}

/// An unencrypted memo received alongside a shielded note in a Zcash transaction.
#[derive(Clone)]
pub struct Memo([u8; 512]);

impl fmt::Debug for Memo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Memo(")?;
        match self.to_utf8() {
            Some(Ok(memo)) => write!(f, "\"{}\"", memo)?,
            _ => fmt_colon_delimited_hex(f, &self.0[..])?,
        }
        write!(f, ")")
    }
}

impl Default for Memo {
    fn default() -> Self {
        // Empty memo field indication per ZIP 302
        let mut memo = [0u8; 512];
        memo[0] = 0xF6;
        Memo(memo)
    }
}

impl PartialEq for Memo {
    fn eq(&self, rhs: &Memo) -> bool {
        self.0[..] == rhs.0[..]
    }
}

impl Memo {
    /// Returns a `Memo` containing the given slice, appending with zero bytes if
    /// necessary, or `None` if the slice is too long. If the slice is empty,
    /// `Memo::default` is returned.
    pub fn from_bytes(memo: &[u8]) -> Option<Memo> {
        if memo.is_empty() {
            Some(Memo::default())
        } else if memo.len() <= 512 {
            let mut data = [0; 512];
            data[0..memo.len()].copy_from_slice(memo);
            Some(Memo(data))
        } else {
            // memo is too long
            None
        }
    }

    /// Returns the underlying bytes of the `Memo`.
    pub fn as_bytes(&self) -> &[u8] {
        &self.0[..]
    }

    /// Returns:
    /// - `None` if the memo is not text
    /// - `Some(Ok(memo))` if the memo contains a valid UTF-8 string
    /// - `Some(Err(e))` if the memo contains invalid UTF-8
    pub fn to_utf8(&self) -> Option<Result<String, str::Utf8Error>> {
        // Check if it is a text or binary memo
        if self.0[0] < 0xF5 {
            // Check if it is valid UTF8
            Some(str::from_utf8(&self.0).map(|memo| {
                // Drop trailing zeroes
                memo.trim_end_matches(char::from(0)).to_owned()
            }))
        } else {
            None
        }
    }
}

impl str::FromStr for Memo {
    type Err = ();

    /// Returns a `Memo` containing the given string, or an error if the string is too long.
    fn from_str(memo: &str) -> Result<Self, Self::Err> {
        Memo::from_bytes(memo.as_bytes()).ok_or(())
    }
}

/// Sapling key agreement for note encryption.
///
/// Implements section 5.4.4.3 of the Zcash Protocol Specification.
pub fn sapling_ka_agree<'a, P>(esk: &Fs, pk_d: &'a P) -> edwards::Point<Bls12, PrimeOrder>
where
    edwards::Point<Bls12, Unknown>: From<&'a P>,
{
    let p: edwards::Point<Bls12, Unknown> = pk_d.into();

    // Multiply by 8
    let p = p.mul_by_cofactor(&JUBJUB);

    // Multiply by esk
    p.mul(*esk, &JUBJUB)
}

/// Sapling KDF for note encryption.
///
/// Implements section 5.4.4.4 of the Zcash Protocol Specification.
fn kdf_sapling(
    dhsecret: edwards::Point<Bls12, PrimeOrder>,
    epk: &edwards::Point<Bls12, PrimeOrder>,
) -> Blake2bHash {
    let mut input = [0u8; 64];
    dhsecret.write(&mut input[0..32]).unwrap();
    epk.write(&mut input[32..64]).unwrap();

    Blake2bParams::new()
        .hash_length(32)
        .personal(KDF_SAPLING_PERSONALIZATION)
        .hash(&input)
}

/// Sapling PRF^ock.
///
/// Implemented per section 5.4.2 of the Zcash Protocol Specification.
pub fn prf_ock(
    ovk: &OutgoingViewingKey,
    cv: &edwards::Point<Bls12, Unknown>,
    cmu: &Fr,
    epk: &edwards::Point<Bls12, PrimeOrder>,
) -> Blake2bHash {
    let mut ock_input = [0u8; 128];
    ock_input[0..32].copy_from_slice(&ovk.0);
    cv.write(&mut ock_input[32..64]).unwrap();
    ock_input[64..96].copy_from_slice(cmu.to_repr().as_ref());
    epk.write(&mut ock_input[96..128]).unwrap();

    Blake2bParams::new()
        .hash_length(32)
        .personal(PRF_OCK_PERSONALIZATION)
        .hash(&ock_input)
}

/// An API for encrypting Sapling notes.
///
/// This struct provides a safe API for encrypting Sapling notes. In particular, it
/// enforces that fresh ephemeral keys are used for every note, and that the ciphertexts
/// are consistent with each other.
///
/// Implements section 4.17.1 of the Zcash Protocol Specification.
/// NB: the example code is only covering the pre-Canopy case.
///
/// # Examples
///
/// ```
/// extern crate ff;
/// extern crate pairing;
/// extern crate rand_core;
/// extern crate zcash_primitives;
///
/// use ff::Field;
/// use pairing::bls12_381::Bls12;
/// use rand_core::OsRng;
/// use zcash_primitives::{
///     jubjub::fs::Fs,
///     keys::{OutgoingViewingKey, prf_expand},
///     note_encryption::{Memo, SaplingNoteEncryption},
///     primitives::{Diversifier, PaymentAddress, Rseed, ValueCommitment},
///     JUBJUB,
/// };
///
/// let mut rng = OsRng;
///
/// let diversifier = Diversifier([0; 11]);
/// let pk_d = diversifier.g_d::<Bls12>(&JUBJUB).unwrap();
/// let to = PaymentAddress::from_parts(diversifier, pk_d).unwrap();
/// let ovk = OutgoingViewingKey([0; 32]);
///
/// let value = 1000;
/// let rcv = Fs::random(&mut rng);
/// let cv = ValueCommitment::<Bls12> {
///     value,
///     randomness: rcv.clone(),
/// };
/// let rcm = Fs::random(&mut rng);
/// let note = to.create_note(value, Rseed::BeforeZip212(rcm), &JUBJUB).unwrap();
/// let cmu = note.cm(&JUBJUB);
///
/// let enc = SaplingNoteEncryption::new(ovk, note, to, Memo::default(), &mut rng);
/// let encCiphertext = enc.encrypt_note_plaintext();
/// let outCiphertext = enc.encrypt_outgoing_plaintext(&cv.cm(&JUBJUB).into(), &cmu);
/// ```
pub struct SaplingNoteEncryption {
    epk: edwards::Point<Bls12, PrimeOrder>,
    esk: Fs,
    note: Note<Bls12>,
    to: PaymentAddress<Bls12>,
    memo: Memo,
    ovk: OutgoingViewingKey,
}

impl SaplingNoteEncryption {
    /// Creates a new encryption context for the given note.
    pub fn new<R: RngCore + CryptoRng>(
        ovk: OutgoingViewingKey,
        note: Note<Bls12>,
        to: PaymentAddress<Bls12>,
        memo: Memo,
        rng: &mut R,
    ) -> SaplingNoteEncryption {
        let esk = note.generate_or_derive_esk(rng);
        let epk = note.g_d.mul(esk, &JUBJUB);

        SaplingNoteEncryption {
            epk,
            esk,
            note,
            to,
            memo,
            ovk,
        }
    }

    /// Exposes the ephemeral secret key being used to encrypt this note.
    pub fn esk(&self) -> &Fs {
        &self.esk
    }

    /// Exposes the ephemeral public key being used to encrypt this note.
    pub fn epk(&self) -> &edwards::Point<Bls12, PrimeOrder> {
        &self.epk
    }

    /// Generates `encCiphertext` for this note.
    pub fn encrypt_note_plaintext(&self) -> [u8; ENC_CIPHERTEXT_SIZE] {
        let shared_secret = sapling_ka_agree(&self.esk, self.to.pk_d());
        let key = kdf_sapling(shared_secret, &self.epk);

        // Note plaintext encoding is defined in section 5.5 of the Zcash Protocol
        // Specification.
        let mut input = [0; NOTE_PLAINTEXT_SIZE];
        input[0] = match self.note.rseed {
            Rseed::BeforeZip212(_) => 1,
            Rseed::AfterZip212(_) => 2,
        };
        input[1..12].copy_from_slice(&self.to.diversifier().0);
        (&mut input[12..20])
            .write_u64::<LittleEndian>(self.note.value)
            .unwrap();
        match self.note.rseed {
            Rseed::BeforeZip212(rcm) => {
                input[20..COMPACT_NOTE_SIZE].copy_from_slice(rcm.to_repr().as_ref());
            }
            Rseed::AfterZip212(rseed) => {
                input[20..COMPACT_NOTE_SIZE].copy_from_slice(&rseed);
            }
        }
        input[COMPACT_NOTE_SIZE..NOTE_PLAINTEXT_SIZE].copy_from_slice(&self.memo.0);

        let mut output = [0u8; ENC_CIPHERTEXT_SIZE];
        assert_eq!(
            ChachaPolyIetf::aead_cipher()
                .seal_to(&mut output, &input, &[], &key.as_bytes(), &[0u8; 12])
                .unwrap(),
            ENC_CIPHERTEXT_SIZE
        );

        output
    }

    /// Generates `outCiphertext` for this note.
    pub fn encrypt_outgoing_plaintext(
        &self,
        cv: &edwards::Point<Bls12, Unknown>,
        cmu: &Fr,
    ) -> [u8; OUT_CIPHERTEXT_SIZE] {
        let key = prf_ock(&self.ovk, &cv, &cmu, &self.epk);

        let mut input = [0u8; OUT_PLAINTEXT_SIZE];
        self.note.pk_d.write(&mut input[0..32]).unwrap();
        input[32..OUT_PLAINTEXT_SIZE].copy_from_slice(self.esk.to_repr().as_ref());

        let mut output = [0u8; OUT_CIPHERTEXT_SIZE];
        assert_eq!(
            ChachaPolyIetf::aead_cipher()
                .seal_to(&mut output, &input, &[], key.as_bytes(), &[0u8; 12])
                .unwrap(),
            OUT_CIPHERTEXT_SIZE
        );

        output
    }
}

fn parse_note_plaintext_without_memo<P: consensus::Parameters>(
    height: u32,
    ivk: &Fs,
    epk: &edwards::Point<Bls12, PrimeOrder>,
    cmu: &Fr,
    plaintext: &[u8],
) -> Option<(Note<Bls12>, PaymentAddress<Bls12>)> {
    // Check note plaintext version
    if !plaintext_version_is_valid::<P>(height, plaintext[0]) {
        return None;
    }

    let mut d = [0u8; 11];
    d.copy_from_slice(&plaintext[1..12]);

    let v = (&plaintext[12..20]).read_u64::<LittleEndian>().ok()?;

    let r: [u8; 32] = plaintext[20..COMPACT_NOTE_SIZE]
        .try_into()
        .expect("slice is the correct length");

    let rseed = if plaintext[0] == 0x01 {
        let rcm = Fs::from_repr(FsRepr(r))?;
        Rseed::BeforeZip212(rcm)
    } else {
        Rseed::AfterZip212(r)
    };

    let diversifier = Diversifier(d);
    let pk_d = diversifier
        .g_d::<Bls12>(&JUBJUB)?
        .mul(ivk.to_repr(), &JUBJUB);

    let to = PaymentAddress::from_parts(diversifier, pk_d)?;
    let note = to.create_note(v, rseed, &JUBJUB).unwrap();

    if note.cm(&JUBJUB) != *cmu {
        // Published commitment doesn't match calculated commitment
        return None;
    }

    if let Some(derived_esk) = note.derive_esk() {
        if note.g_d.mul(derived_esk, &JUBJUB) != *epk {
            return None;
        }
    }

    Some((note, to))
}

pub fn plaintext_version_is_valid<P: consensus::Parameters>(height: u32, leadbyte: u8) -> bool {
    if P::is_nu_active(NetworkUpgrade::Canopy, height) {
        let grace_period_end_height = P::activation_height(NetworkUpgrade::Canopy)
            .expect("Should have Canopy activation height")
            + ZIP212_GRACE_PERIOD;

        if height < grace_period_end_height && leadbyte != 0x01 && leadbyte != 0x02 {
            // non-{0x01,0x02} received after Canopy activation and before grace period has elapsed
            false
        } else if height >= grace_period_end_height && leadbyte != 0x02 {
            // non-0x02 received past (Canopy activation height + grace period)
            false
        } else {
            true
        }
    } else {
        // return false if non-0x01 received when Canopy is not active
        leadbyte == 0x01
    }
}

/// Trial decryption of the full note plaintext by the recipient.
///
/// Attempts to decrypt and validate the given `enc_ciphertext` using the given `ivk`.
/// If successful, the corresponding Sapling note and memo are returned, along with the
/// `PaymentAddress` to which the note was sent.
///
/// Implements section 4.17.2 of the Zcash Protocol Specification.
pub fn try_sapling_note_decryption<P: consensus::Parameters>(
    height: u32,
    ivk: &Fs,
    epk: &edwards::Point<Bls12, PrimeOrder>,
    cmu: &Fr,
    enc_ciphertext: &[u8],
) -> Option<(Note<Bls12>, PaymentAddress<Bls12>, Memo)> {
    assert_eq!(enc_ciphertext.len(), ENC_CIPHERTEXT_SIZE);

    let shared_secret = sapling_ka_agree(ivk, epk);
    let key = kdf_sapling(shared_secret, &epk);

    let mut plaintext = [0; ENC_CIPHERTEXT_SIZE];
    assert_eq!(
        ChachaPolyIetf::aead_cipher()
            .open_to(
                &mut plaintext,
                &enc_ciphertext,
                &[],
                key.as_bytes(),
                &[0u8; 12]
            )
            .ok()?,
        NOTE_PLAINTEXT_SIZE
    );

    let (note, to) = parse_note_plaintext_without_memo::<P>(height, ivk, epk, cmu, &plaintext)?;

    let mut memo = [0u8; 512];
    memo.copy_from_slice(&plaintext[COMPACT_NOTE_SIZE..NOTE_PLAINTEXT_SIZE]);

    Some((note, to, Memo(memo)))
}

/// Trial decryption of the compact note plaintext by the recipient for light clients.
///
/// Attempts to decrypt and validate the first 52 bytes of `enc_ciphertext` using the
/// given `ivk`. If successful, the corresponding Sapling note is returned, along with the
/// `PaymentAddress` to which the note was sent.
///
/// Implements the procedure specified in [`ZIP 307`].
///
/// [`ZIP 307`]: https://zips.z.cash/zip-0307
pub fn try_sapling_compact_note_decryption<P: consensus::Parameters>(
    height: u32,
    ivk: &Fs,
    epk: &edwards::Point<Bls12, PrimeOrder>,
    cmu: &Fr,
    enc_ciphertext: &[u8],
) -> Option<(Note<Bls12>, PaymentAddress<Bls12>)> {
    assert_eq!(enc_ciphertext.len(), COMPACT_NOTE_SIZE);

    let shared_secret = sapling_ka_agree(ivk, epk);
    let key = kdf_sapling(shared_secret, &epk);

    // Start from block 1 to skip over Poly1305 keying output
    let mut plaintext = [0; COMPACT_NOTE_SIZE];
    plaintext.copy_from_slice(&enc_ciphertext);
    ChaCha20Ietf::xor(key.as_bytes(), &[0u8; 12], 1, &mut plaintext);

    parse_note_plaintext_without_memo::<P>(height, ivk, epk, cmu, &plaintext)
}

/// Recovery of the full note plaintext by the sender.
///
/// Attempts to decrypt and validate the given `enc_ciphertext` using the given `ock`.
/// If successful, the corresponding Sapling note and memo are returned, along with the
/// `PaymentAddress` to which the note was sent.
///
/// Implements part of section 4.17.3 of the Zcash Protocol Specification.
/// For decryption using a Full Viewing Key see [`try_sapling_output_recovery`].
pub fn try_sapling_output_recovery_with_ock<P: consensus::Parameters>(
    height: u32,
    ock: &[u8],
    cmu: &Fr,
    epk: &edwards::Point<Bls12, PrimeOrder>,
    enc_ciphertext: &[u8],
    out_ciphertext: &[u8],
) -> Option<(Note<Bls12>, PaymentAddress<Bls12>, Memo)> {
    assert_eq!(enc_ciphertext.len(), ENC_CIPHERTEXT_SIZE);
    assert_eq!(out_ciphertext.len(), OUT_CIPHERTEXT_SIZE);

    let mut op = [0; OUT_CIPHERTEXT_SIZE];
    assert_eq!(
        ChachaPolyIetf::aead_cipher()
            .open_to(&mut op, &out_ciphertext, &[], &ock, &[0u8; 12])
            .ok()?,
        OUT_PLAINTEXT_SIZE
    );

    let pk_d = edwards::Point::<Bls12, _>::read(&op[0..32], &JUBJUB)
        .ok()?
        .as_prime_order(&JUBJUB)?;

    let esk = Fs::from_repr(FsRepr(
        op[32..OUT_PLAINTEXT_SIZE]
            .try_into()
            .expect("slice is the correct length"),
    ))?;

    let shared_secret = sapling_ka_agree(&esk, &pk_d);
    let key = kdf_sapling(shared_secret, &epk);

    let mut plaintext = [0; ENC_CIPHERTEXT_SIZE];
    assert_eq!(
        ChachaPolyIetf::aead_cipher()
            .open_to(
                &mut plaintext,
                &enc_ciphertext,
                &[],
                key.as_bytes(),
                &[0u8; 12]
            )
            .ok()?,
        NOTE_PLAINTEXT_SIZE
    );

    // Check note plaintext version
    if !plaintext_version_is_valid::<P>(height, plaintext[0]) {
        return None;
    }

    let mut d = [0u8; 11];
    d.copy_from_slice(&plaintext[1..12]);

    let v = (&plaintext[12..20]).read_u64::<LittleEndian>().ok()?;

    let r: [u8; 32] = plaintext[20..COMPACT_NOTE_SIZE]
        .try_into()
        .expect("slice is the correct length");

    let rseed = if plaintext[0] == 0x01 {
        let rcm = Fs::from_repr(FsRepr(r))?;
        Rseed::BeforeZip212(rcm)
    } else {
        Rseed::AfterZip212(r)
    };

    let mut memo = [0u8; 512];
    memo.copy_from_slice(&plaintext[COMPACT_NOTE_SIZE..NOTE_PLAINTEXT_SIZE]);

    let diversifier = Diversifier(d);
    if diversifier
        .g_d::<Bls12>(&JUBJUB)?
        .mul(esk.to_repr(), &JUBJUB)
        != *epk
    {
        // Published epk doesn't match calculated epk
        return None;
    }

    let to = PaymentAddress::from_parts(diversifier, pk_d)?;
    let note = to.create_note(v, rseed, &JUBJUB).unwrap();

    if note.cm(&JUBJUB) != *cmu {
        // Published commitment doesn't match calculated commitment
        return None;
    }

    if let Some(derived_esk) = note.derive_esk() {
        if derived_esk != esk {
            return None;
        }
    }

    Some((note, to, Memo(memo)))
}

/// Recovery of the full note plaintext by the sender.
///
/// Attempts to decrypt and validate the given `enc_ciphertext` using the given `ovk`.
/// If successful, the corresponding Sapling note and memo are returned, along with the
/// `PaymentAddress` to which the note was sent.
///
/// Implements section 4.17.3 of the Zcash Protocol Specification.
pub fn try_sapling_output_recovery<P: consensus::Parameters>(
    height: u32,
    ovk: &OutgoingViewingKey,
    cv: &edwards::Point<Bls12, Unknown>,
    cmu: &Fr,
    epk: &edwards::Point<Bls12, PrimeOrder>,
    enc_ciphertext: &[u8],
    out_ciphertext: &[u8],
) -> Option<(Note<Bls12>, PaymentAddress<Bls12>, Memo)> {
    try_sapling_output_recovery_with_ock::<P>(
        height,
        prf_ock(&ovk, &cv, &cmu, &epk).as_bytes(),
        cmu,
        epk,
        enc_ciphertext,
        out_ciphertext,
    )
}

#[cfg(test)]
mod tests {
    use crate::{
        consensus::{
            NetworkUpgrade,
            NetworkUpgrade::{Canopy, Sapling},
            Parameters, TestNetwork, ZIP212_GRACE_PERIOD,
        },
        jubjub::{
            edwards,
            fs::{Fs, FsRepr},
            PrimeOrder, Unknown,
        },
        primitives::{Diversifier, PaymentAddress, Rseed, ValueCommitment},
        util::generate_random_rseed,
    };
    use blake2b_simd::Hash as Blake2bHash;
    use crypto_api_chachapoly::ChachaPolyIetf;
    use ff::{Field, PrimeField};
    use pairing::bls12_381::{Bls12, Fr, FrRepr};
    use rand_core::OsRng;
    use rand_core::{CryptoRng, RngCore};
    use std::convert::TryInto;
    use std::str::FromStr;

    use super::{
        kdf_sapling, prf_ock, sapling_ka_agree, try_sapling_compact_note_decryption,
        try_sapling_note_decryption, try_sapling_output_recovery,
        try_sapling_output_recovery_with_ock, Memo, SaplingNoteEncryption, COMPACT_NOTE_SIZE,
        ENC_CIPHERTEXT_SIZE, NOTE_PLAINTEXT_SIZE, OUT_CIPHERTEXT_SIZE, OUT_PLAINTEXT_SIZE,
    };
    use crate::{keys::OutgoingViewingKey, JUBJUB};

    #[test]
    fn memo_from_str() {
        assert_eq!(
            Memo::from_str("").unwrap(),
            Memo([
                0xf6, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00
            ])
        );
        assert_eq!(
            Memo::from_str(
                "thiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiis \
                 iiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiis \
                 aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa \
                 veeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeryyyyyyyyyyyyyyyyyyyyyyyyyy \
                 looooooooooooooooooooooooooooooooooooooooooooooooooooooooooooooooooooooooooooong \
                 meeeeeeeeeeeeeeeeeeemooooooooooooooooooooooooooooooooooooooooooooooooooooooooooo \
                 but it's just short enough"
            )
            .unwrap(),
            Memo([
                0x74, 0x68, 0x69, 0x69, 0x69, 0x69, 0x69, 0x69, 0x69, 0x69, 0x69, 0x69, 0x69, 0x69,
                0x69, 0x69, 0x69, 0x69, 0x69, 0x69, 0x69, 0x69, 0x69, 0x69, 0x69, 0x69, 0x69, 0x69,
                0x69, 0x69, 0x69, 0x69, 0x69, 0x69, 0x69, 0x69, 0x69, 0x69, 0x69, 0x69, 0x69, 0x69,
                0x69, 0x69, 0x69, 0x69, 0x69, 0x69, 0x69, 0x69, 0x69, 0x69, 0x69, 0x69, 0x69, 0x69,
                0x69, 0x69, 0x69, 0x69, 0x69, 0x69, 0x69, 0x69, 0x69, 0x69, 0x69, 0x69, 0x69, 0x69,
                0x69, 0x69, 0x69, 0x69, 0x69, 0x69, 0x69, 0x69, 0x69, 0x73, 0x20, 0x69, 0x69, 0x69,
                0x69, 0x69, 0x69, 0x69, 0x69, 0x69, 0x69, 0x69, 0x69, 0x69, 0x69, 0x69, 0x69, 0x69,
                0x69, 0x69, 0x69, 0x69, 0x69, 0x69, 0x69, 0x69, 0x69, 0x69, 0x69, 0x69, 0x69, 0x69,
                0x69, 0x69, 0x69, 0x69, 0x69, 0x69, 0x69, 0x69, 0x69, 0x69, 0x69, 0x69, 0x69, 0x69,
                0x69, 0x69, 0x69, 0x69, 0x69, 0x69, 0x69, 0x69, 0x69, 0x69, 0x69, 0x69, 0x69, 0x69,
                0x69, 0x69, 0x69, 0x69, 0x69, 0x69, 0x69, 0x69, 0x69, 0x69, 0x69, 0x69, 0x69, 0x69,
                0x69, 0x69, 0x69, 0x69, 0x69, 0x69, 0x73, 0x20, 0x61, 0x61, 0x61, 0x61, 0x61, 0x61,
                0x61, 0x61, 0x61, 0x61, 0x61, 0x61, 0x61, 0x61, 0x61, 0x61, 0x61, 0x61, 0x61, 0x61,
                0x61, 0x61, 0x61, 0x61, 0x61, 0x61, 0x61, 0x61, 0x61, 0x61, 0x61, 0x61, 0x61, 0x61,
                0x61, 0x61, 0x61, 0x61, 0x61, 0x61, 0x61, 0x61, 0x61, 0x61, 0x61, 0x61, 0x61, 0x61,
                0x61, 0x61, 0x61, 0x61, 0x61, 0x61, 0x61, 0x61, 0x61, 0x61, 0x61, 0x61, 0x61, 0x61,
                0x61, 0x61, 0x61, 0x61, 0x61, 0x61, 0x61, 0x61, 0x61, 0x61, 0x61, 0x61, 0x61, 0x61,
                0x61, 0x61, 0x61, 0x61, 0x20, 0x76, 0x65, 0x65, 0x65, 0x65, 0x65, 0x65, 0x65, 0x65,
                0x65, 0x65, 0x65, 0x65, 0x65, 0x65, 0x65, 0x65, 0x65, 0x65, 0x65, 0x65, 0x65, 0x65,
                0x65, 0x65, 0x65, 0x65, 0x65, 0x65, 0x65, 0x65, 0x65, 0x65, 0x65, 0x65, 0x65, 0x65,
                0x65, 0x65, 0x65, 0x65, 0x65, 0x65, 0x65, 0x65, 0x65, 0x65, 0x65, 0x65, 0x65, 0x65,
                0x65, 0x65, 0x72, 0x79, 0x79, 0x79, 0x79, 0x79, 0x79, 0x79, 0x79, 0x79, 0x79, 0x79,
                0x79, 0x79, 0x79, 0x79, 0x79, 0x79, 0x79, 0x79, 0x79, 0x79, 0x79, 0x79, 0x79, 0x79,
                0x79, 0x20, 0x6c, 0x6f, 0x6f, 0x6f, 0x6f, 0x6f, 0x6f, 0x6f, 0x6f, 0x6f, 0x6f, 0x6f,
                0x6f, 0x6f, 0x6f, 0x6f, 0x6f, 0x6f, 0x6f, 0x6f, 0x6f, 0x6f, 0x6f, 0x6f, 0x6f, 0x6f,
                0x6f, 0x6f, 0x6f, 0x6f, 0x6f, 0x6f, 0x6f, 0x6f, 0x6f, 0x6f, 0x6f, 0x6f, 0x6f, 0x6f,
                0x6f, 0x6f, 0x6f, 0x6f, 0x6f, 0x6f, 0x6f, 0x6f, 0x6f, 0x6f, 0x6f, 0x6f, 0x6f, 0x6f,
                0x6f, 0x6f, 0x6f, 0x6f, 0x6f, 0x6f, 0x6f, 0x6f, 0x6f, 0x6f, 0x6f, 0x6f, 0x6f, 0x6f,
                0x6f, 0x6f, 0x6f, 0x6f, 0x6f, 0x6f, 0x6f, 0x6f, 0x6f, 0x6f, 0x6e, 0x67, 0x20, 0x6d,
                0x65, 0x65, 0x65, 0x65, 0x65, 0x65, 0x65, 0x65, 0x65, 0x65, 0x65, 0x65, 0x65, 0x65,
                0x65, 0x65, 0x65, 0x65, 0x65, 0x6d, 0x6f, 0x6f, 0x6f, 0x6f, 0x6f, 0x6f, 0x6f, 0x6f,
                0x6f, 0x6f, 0x6f, 0x6f, 0x6f, 0x6f, 0x6f, 0x6f, 0x6f, 0x6f, 0x6f, 0x6f, 0x6f, 0x6f,
                0x6f, 0x6f, 0x6f, 0x6f, 0x6f, 0x6f, 0x6f, 0x6f, 0x6f, 0x6f, 0x6f, 0x6f, 0x6f, 0x6f,
                0x6f, 0x6f, 0x6f, 0x6f, 0x6f, 0x6f, 0x6f, 0x6f, 0x6f, 0x6f, 0x6f, 0x6f, 0x6f, 0x6f,
                0x6f, 0x6f, 0x6f, 0x6f, 0x6f, 0x6f, 0x6f, 0x6f, 0x6f, 0x20, 0x62, 0x75, 0x74, 0x20,
                0x69, 0x74, 0x27, 0x73, 0x20, 0x6a, 0x75, 0x73, 0x74, 0x20, 0x73, 0x68, 0x6f, 0x72,
                0x74, 0x20, 0x65, 0x6e, 0x6f, 0x75, 0x67, 0x68
            ])
        );
        assert_eq!(
            Memo::from_str(
                "thiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiis \
                 iiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiis \
                 aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa \
                 veeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeryyyyyyyyyyyyyyyyyyyyyyyyyy \
                 looooooooooooooooooooooooooooooooooooooooooooooooooooooooooooooooooooooooooooong \
                 meeeeeeeeeeeeeeeeeeemooooooooooooooooooooooooooooooooooooooooooooooooooooooooooo \
                 but it's now a bit too long"
            ),
            Err(())
        );
    }

    #[test]
    fn memo_to_utf8() {
        let memo = Memo::from_str("Test memo").unwrap();
        assert_eq!(memo.to_utf8(), Some(Ok("Test memo".to_owned())));
        assert_eq!(Memo::default().to_utf8(), None);
    }

    fn random_enc_ciphertext<R: RngCore + CryptoRng>(
        height: u32,
        mut rng: &mut R,
    ) -> (
        OutgoingViewingKey,
        Blake2bHash,
        Fs,
        edwards::Point<Bls12, Unknown>,
        Fr,
        edwards::Point<Bls12, PrimeOrder>,
        [u8; ENC_CIPHERTEXT_SIZE],
        [u8; OUT_CIPHERTEXT_SIZE],
    ) {
        let ivk = Fs::random(&mut rng);

        let (ovk, ock, ivk, cv, cmu, epk, enc_ciphertext, out_ciphertext) =
            random_enc_ciphertext_with(height, ivk, rng);

        assert!(try_sapling_note_decryption::<TestNetwork>(
            height,
            &ivk,
            &epk,
            &cmu,
            &enc_ciphertext
        )
        .is_some());
        assert!(try_sapling_compact_note_decryption::<TestNetwork>(
            height,
            &ivk,
            &epk,
            &cmu,
            &enc_ciphertext[..COMPACT_NOTE_SIZE]
        )
        .is_some());

        let ovk_output_recovery = try_sapling_output_recovery::<TestNetwork>(
            height,
            &ovk,
            &cv,
            &cmu,
            &epk,
            &enc_ciphertext,
            &out_ciphertext,
        );
        let ock_output_recovery = try_sapling_output_recovery_with_ock::<TestNetwork>(
            height,
            ock.as_bytes(),
            &cmu,
            &epk,
            &enc_ciphertext,
            &out_ciphertext,
        );
        assert!(ovk_output_recovery.is_some());
        assert!(ock_output_recovery.is_some());
        assert_eq!(ovk_output_recovery, ock_output_recovery);

        (ovk, ock, ivk, cv, cmu, epk, enc_ciphertext, out_ciphertext)
    }

    fn random_enc_ciphertext_with<R: RngCore + CryptoRng>(
        height: u32,
        ivk: Fs,
        mut rng: &mut R,
    ) -> (
        OutgoingViewingKey,
        Blake2bHash,
        Fs,
        edwards::Point<Bls12, Unknown>,
        Fr,
        edwards::Point<Bls12, PrimeOrder>,
        [u8; ENC_CIPHERTEXT_SIZE],
        [u8; OUT_CIPHERTEXT_SIZE],
    ) {
        let diversifier = Diversifier([0; 11]);
        let pk_d = diversifier.g_d::<Bls12>(&JUBJUB).unwrap().mul(ivk, &JUBJUB);
        let pa = PaymentAddress::from_parts_unchecked(diversifier, pk_d);

        // Construct the value commitment for the proof instance
        let value = 100;
        let value_commitment = ValueCommitment::<Bls12> {
            value,
            randomness: Fs::random(&mut rng),
        };
        let cv = value_commitment.cm(&JUBJUB).into();

        let rseed = generate_random_rseed::<TestNetwork, R>(height, &mut rng);

        let note = pa.create_note(value, rseed, &JUBJUB).unwrap();
        let cmu = note.cm(&JUBJUB);

        let ovk = OutgoingViewingKey([0; 32]);
        let ne = SaplingNoteEncryption::new(ovk, note, pa, Memo([0; 512]), &mut rng);
        let epk = ne.epk();
        let enc_ciphertext = ne.encrypt_note_plaintext();
        let out_ciphertext = ne.encrypt_outgoing_plaintext(&cv, &cmu);
        let ock = prf_ock(&ovk, &cv, &cmu, &epk);

        (
            ovk,
            ock,
            ivk,
            cv,
            cmu,
            epk.clone(),
            enc_ciphertext,
            out_ciphertext,
        )
    }

    fn reencrypt_enc_ciphertext(
        ovk: &OutgoingViewingKey,
        cv: &edwards::Point<Bls12, Unknown>,
        cmu: &Fr,
        epk: &edwards::Point<Bls12, PrimeOrder>,
        enc_ciphertext: &mut [u8; ENC_CIPHERTEXT_SIZE],
        out_ciphertext: &[u8; OUT_CIPHERTEXT_SIZE],
        modify_plaintext: impl Fn(&mut [u8; NOTE_PLAINTEXT_SIZE]),
    ) {
        let ock = prf_ock(&ovk, &cv, &cmu, &epk);

        let mut op = [0; OUT_CIPHERTEXT_SIZE];
        assert_eq!(
            ChachaPolyIetf::aead_cipher()
                .open_to(&mut op, out_ciphertext, &[], ock.as_bytes(), &[0u8; 12])
                .unwrap(),
            OUT_PLAINTEXT_SIZE
        );

        let pk_d = edwards::Point::<Bls12, _>::read(&op[0..32], &JUBJUB)
            .unwrap()
            .as_prime_order(&JUBJUB)
            .unwrap();

        let esk = Fs::from_repr(FsRepr(op[32..OUT_PLAINTEXT_SIZE].try_into().unwrap())).unwrap();

        let shared_secret = sapling_ka_agree(&esk, &pk_d);
        let key = kdf_sapling(shared_secret, &epk);

        let mut plaintext = {
            let mut buf = [0; ENC_CIPHERTEXT_SIZE];
            assert_eq!(
                ChachaPolyIetf::aead_cipher()
                    .open_to(&mut buf, enc_ciphertext, &[], key.as_bytes(), &[0u8; 12])
                    .unwrap(),
                NOTE_PLAINTEXT_SIZE
            );
            let mut pt = [0; NOTE_PLAINTEXT_SIZE];
            pt.copy_from_slice(&buf[..NOTE_PLAINTEXT_SIZE]);
            pt
        };

        modify_plaintext(&mut plaintext);

        assert_eq!(
            ChachaPolyIetf::aead_cipher()
                .seal_to(enc_ciphertext, &plaintext, &[], &key.as_bytes(), &[0u8; 12])
                .unwrap(),
            ENC_CIPHERTEXT_SIZE
        );
    }

    fn find_invalid_diversifier() -> Diversifier {
        // Find an invalid diversifier
        let mut d = Diversifier([0; 11]);
        loop {
            for k in 0..11 {
                d.0[k] = d.0[k].wrapping_add(1);
                if d.0[k] != 0 {
                    break;
                }
            }
            if d.g_d::<Bls12>(&JUBJUB).is_none() {
                break;
            }
        }
        d
    }

    fn find_valid_diversifier() -> Diversifier {
        // Find a different valid diversifier
        let mut d = Diversifier([0; 11]);
        loop {
            for k in 0..11 {
                d.0[k] = d.0[k].wrapping_add(1);
                if d.0[k] != 0 {
                    break;
                }
            }
            if d.g_d::<Bls12>(&JUBJUB).is_some() {
                break;
            }
        }
        d
    }

    #[test]
    fn decryption_with_invalid_ivk() {
        let mut rng = OsRng;
        let heights = [
            TestNetwork::activation_height(Sapling).unwrap(),
            TestNetwork::activation_height(Canopy).unwrap(),
        ];

        for &height in heights.iter() {
            let (_, _, _, _, cmu, epk, enc_ciphertext, _) = random_enc_ciphertext(height, &mut rng);

            assert_eq!(
                try_sapling_note_decryption::<TestNetwork>(
                    height,
                    &Fs::random(&mut rng),
                    &epk,
                    &cmu,
                    &enc_ciphertext
                ),
                None
            );
        }
    }

    #[test]
    fn decryption_with_invalid_epk() {
        let mut rng = OsRng;
        let heights = [
            TestNetwork::activation_height(Sapling).unwrap(),
            TestNetwork::activation_height(Canopy).unwrap(),
        ];

        for &height in heights.iter() {
            let (_, _, ivk, _, cmu, _, enc_ciphertext, _) = random_enc_ciphertext(height, &mut rng);

            assert_eq!(
                try_sapling_note_decryption::<TestNetwork>(
                    height,
                    &ivk,
                    &edwards::Point::<Bls12, _>::rand(&mut rng, &JUBJUB).mul_by_cofactor(&JUBJUB),
                    &cmu,
                    &enc_ciphertext
                ),
                None
            );
        }
    }

    #[test]
    fn decryption_with_invalid_cmu() {
        let mut rng = OsRng;
        let heights = [
            TestNetwork::activation_height(Sapling).unwrap(),
            TestNetwork::activation_height(Canopy).unwrap(),
        ];

        for &height in heights.iter() {
            let (_, _, ivk, _, _, epk, enc_ciphertext, _) = random_enc_ciphertext(height, &mut rng);

            assert_eq!(
                try_sapling_note_decryption::<TestNetwork>(
                    height,
                    &ivk,
                    &epk,
                    &Fr::random(&mut rng),
                    &enc_ciphertext
                ),
                None
            );
        }
    }

    #[test]
    fn decryption_with_invalid_tag() {
        let mut rng = OsRng;
        let heights = [
            TestNetwork::activation_height(Sapling).unwrap(),
            TestNetwork::activation_height(Canopy).unwrap(),
        ];

        for &height in heights.iter() {
            let (_, _, ivk, _, cmu, epk, mut enc_ciphertext, _) =
                random_enc_ciphertext(height, &mut rng);

            enc_ciphertext[ENC_CIPHERTEXT_SIZE - 1] ^= 0xff;
            assert_eq!(
                try_sapling_note_decryption::<TestNetwork>(
                    height,
                    &ivk,
                    &epk,
                    &cmu,
                    &enc_ciphertext
                ),
                None
            );
        }
    }

    #[test]
    fn decryption_with_invalid_version_byte() {
        let mut rng = OsRng;
        let canopy_activation_height = TestNetwork::activation_height(Canopy).unwrap();
        let heights = [
            canopy_activation_height - 1,
            canopy_activation_height,
            canopy_activation_height + ZIP212_GRACE_PERIOD,
        ];
        let leadbytes = [0x02, 0x03, 0x01];

        for (&height, &leadbyte) in heights.iter().zip(leadbytes.iter()) {
            let (ovk, _, ivk, cv, cmu, epk, mut enc_ciphertext, out_ciphertext) =
                random_enc_ciphertext(height, &mut rng);

            reencrypt_enc_ciphertext(
                &ovk,
                &cv,
                &cmu,
                &epk,
                &mut enc_ciphertext,
                &out_ciphertext,
                |pt| pt[0] = leadbyte,
            );
            assert_eq!(
                try_sapling_note_decryption::<TestNetwork>(
                    height,
                    &ivk,
                    &epk,
                    &cmu,
                    &enc_ciphertext
                ),
                None
            );
        }
    }

    #[test]
    fn decryption_with_invalid_diversifier() {
        let mut rng = OsRng;
        let heights = [
            TestNetwork::activation_height(Sapling).unwrap(),
            TestNetwork::activation_height(Canopy).unwrap(),
        ];

        for &height in heights.iter() {
            let (ovk, _, ivk, cv, cmu, epk, mut enc_ciphertext, out_ciphertext) =
                random_enc_ciphertext(height, &mut rng);

            reencrypt_enc_ciphertext(
                &ovk,
                &cv,
                &cmu,
                &epk,
                &mut enc_ciphertext,
                &out_ciphertext,
                |pt| pt[1..12].copy_from_slice(&find_invalid_diversifier().0),
            );
            assert_eq!(
                try_sapling_note_decryption::<TestNetwork>(
                    height,
                    &ivk,
                    &epk,
                    &cmu,
                    &enc_ciphertext
                ),
                None
            );
        }
    }

    #[test]
    fn decryption_with_incorrect_diversifier() {
        let mut rng = OsRng;
        let heights = [
            TestNetwork::activation_height(Sapling).unwrap(),
            TestNetwork::activation_height(Canopy).unwrap(),
        ];

        for &height in heights.iter() {
            let (ovk, _, ivk, cv, cmu, epk, mut enc_ciphertext, out_ciphertext) =
                random_enc_ciphertext(height, &mut rng);

            reencrypt_enc_ciphertext(
                &ovk,
                &cv,
                &cmu,
                &epk,
                &mut enc_ciphertext,
                &out_ciphertext,
                |pt| pt[1..12].copy_from_slice(&find_valid_diversifier().0),
            );
            assert_eq!(
                try_sapling_note_decryption::<TestNetwork>(
                    height,
                    &ivk,
                    &epk,
                    &cmu,
                    &enc_ciphertext
                ),
                None
            );
        }
    }

    #[test]
    fn compact_decryption_with_invalid_ivk() {
        let mut rng = OsRng;
        let heights = [
            TestNetwork::activation_height(Sapling).unwrap(),
            TestNetwork::activation_height(Canopy).unwrap(),
        ];

        for &height in heights.iter() {
            let (_, _, _, _, cmu, epk, enc_ciphertext, _) = random_enc_ciphertext(height, &mut rng);

            assert_eq!(
                try_sapling_compact_note_decryption::<TestNetwork>(
                    height,
                    &Fs::random(&mut rng),
                    &epk,
                    &cmu,
                    &enc_ciphertext[..COMPACT_NOTE_SIZE]
                ),
                None
            );
        }
    }

    #[test]
    fn compact_decryption_with_invalid_epk() {
        let mut rng = OsRng;
        let heights = [
            TestNetwork::activation_height(Sapling).unwrap(),
            TestNetwork::activation_height(Canopy).unwrap(),
        ];

        for &height in heights.iter() {
            let (_, _, ivk, _, cmu, _, enc_ciphertext, _) = random_enc_ciphertext(height, &mut rng);

            assert_eq!(
                try_sapling_compact_note_decryption::<TestNetwork>(
                    height,
                    &ivk,
                    &edwards::Point::<Bls12, _>::rand(&mut rng, &JUBJUB).mul_by_cofactor(&JUBJUB),
                    &cmu,
                    &enc_ciphertext[..COMPACT_NOTE_SIZE]
                ),
                None
            );
        }
    }

    #[test]
    fn compact_decryption_with_invalid_cmu() {
        let mut rng = OsRng;
        let heights = [
            TestNetwork::activation_height(Sapling).unwrap(),
            TestNetwork::activation_height(Canopy).unwrap(),
        ];

        for &height in heights.iter() {
            let (_, _, ivk, _, _, epk, enc_ciphertext, _) = random_enc_ciphertext(height, &mut rng);

            assert_eq!(
                try_sapling_compact_note_decryption::<TestNetwork>(
                    height,
                    &ivk,
                    &epk,
                    &Fr::random(&mut rng),
                    &enc_ciphertext[..COMPACT_NOTE_SIZE]
                ),
                None
            );
        }
    }

    #[test]
    fn compact_decryption_with_invalid_version_byte() {
        let mut rng = OsRng;
        let canopy_activation_height = TestNetwork::activation_height(Canopy).unwrap();
        let heights = [
            canopy_activation_height - 1,
            canopy_activation_height,
            canopy_activation_height + ZIP212_GRACE_PERIOD,
        ];
        let leadbytes = [0x02, 0x03, 0x01];

        for (&height, &leadbyte) in heights.iter().zip(leadbytes.iter()) {
            let (ovk, _, ivk, cv, cmu, epk, mut enc_ciphertext, out_ciphertext) =
                random_enc_ciphertext(height, &mut rng);

            reencrypt_enc_ciphertext(
                &ovk,
                &cv,
                &cmu,
                &epk,
                &mut enc_ciphertext,
                &out_ciphertext,
                |pt| pt[0] = leadbyte,
            );
            assert_eq!(
                try_sapling_compact_note_decryption::<TestNetwork>(
                    height,
                    &ivk,
                    &epk,
                    &cmu,
                    &enc_ciphertext[..COMPACT_NOTE_SIZE]
                ),
                None
            );
        }
    }

    #[test]
    fn compact_decryption_with_invalid_diversifier() {
        let mut rng = OsRng;
        let heights = [
            TestNetwork::activation_height(Sapling).unwrap(),
            TestNetwork::activation_height(Canopy).unwrap(),
        ];

        for &height in heights.iter() {
            let (ovk, _, ivk, cv, cmu, epk, mut enc_ciphertext, out_ciphertext) =
                random_enc_ciphertext(height, &mut rng);

            reencrypt_enc_ciphertext(
                &ovk,
                &cv,
                &cmu,
                &epk,
                &mut enc_ciphertext,
                &out_ciphertext,
                |pt| pt[1..12].copy_from_slice(&find_invalid_diversifier().0),
            );
            assert_eq!(
                try_sapling_compact_note_decryption::<TestNetwork>(
                    height,
                    &ivk,
                    &epk,
                    &cmu,
                    &enc_ciphertext[..COMPACT_NOTE_SIZE]
                ),
                None
            );
        }
    }

    #[test]
    fn compact_decryption_with_incorrect_diversifier() {
        let mut rng = OsRng;
        let heights = [
            TestNetwork::activation_height(Sapling).unwrap(),
            TestNetwork::activation_height(Canopy).unwrap(),
        ];

        for &height in heights.iter() {
            let (ovk, _, ivk, cv, cmu, epk, mut enc_ciphertext, out_ciphertext) =
                random_enc_ciphertext(height, &mut rng);

            reencrypt_enc_ciphertext(
                &ovk,
                &cv,
                &cmu,
                &epk,
                &mut enc_ciphertext,
                &out_ciphertext,
                |pt| pt[1..12].copy_from_slice(&find_valid_diversifier().0),
            );
            assert_eq!(
                try_sapling_compact_note_decryption::<TestNetwork>(
                    height,
                    &ivk,
                    &epk,
                    &cmu,
                    &enc_ciphertext[..COMPACT_NOTE_SIZE]
                ),
                None
            );
        }
    }

    #[test]
    fn recovery_with_invalid_ovk() {
        let mut rng = OsRng;
        let heights = [
            TestNetwork::activation_height(Sapling).unwrap(),
            TestNetwork::activation_height(Canopy).unwrap(),
        ];

        for &height in heights.iter() {
            let (mut ovk, _, _, cv, cmu, epk, enc_ciphertext, out_ciphertext) =
                random_enc_ciphertext(height, &mut rng);

            ovk.0[0] ^= 0xff;
            assert_eq!(
                try_sapling_output_recovery::<TestNetwork>(
                    height,
                    &ovk,
                    &cv,
                    &cmu,
                    &epk,
                    &enc_ciphertext,
                    &out_ciphertext
                ),
                None
            );
        }
    }

    #[test]
    fn recovery_with_invalid_ock() {
        let mut rng = OsRng;
        let heights = [
            TestNetwork::activation_height(Sapling).unwrap(),
            TestNetwork::activation_height(Canopy).unwrap(),
        ];

        for &height in heights.iter() {
            let (_, _, _, _, cmu, epk, enc_ciphertext, out_ciphertext) =
                random_enc_ciphertext(height, &mut rng);

            assert_eq!(
                try_sapling_output_recovery_with_ock::<TestNetwork>(
                    height,
                    &[0u8; 32],
                    &cmu,
                    &epk,
                    &enc_ciphertext,
                    &out_ciphertext
                ),
                None
            );
        }
    }

    #[test]
    fn recovery_with_invalid_cv() {
        let mut rng = OsRng;
        let heights = [
            TestNetwork::activation_height(Sapling).unwrap(),
            TestNetwork::activation_height(Canopy).unwrap(),
        ];

        for &height in heights.iter() {
            let (ovk, _, _, _, cmu, epk, enc_ciphertext, out_ciphertext) =
                random_enc_ciphertext(height, &mut rng);

            assert_eq!(
                try_sapling_output_recovery::<TestNetwork>(
                    height,
                    &ovk,
                    &edwards::Point::<Bls12, _>::rand(&mut rng, &JUBJUB),
                    &cmu,
                    &epk,
                    &enc_ciphertext,
                    &out_ciphertext
                ),
                None
            );
        }
    }

    #[test]
    fn recovery_with_invalid_cmu() {
        let mut rng = OsRng;
        let heights = [
            TestNetwork::activation_height(Sapling).unwrap(),
            TestNetwork::activation_height(Canopy).unwrap(),
        ];

        for &height in heights.iter() {
            let (ovk, ock, _, cv, _, epk, enc_ctext, out_ctext) =
                random_enc_ciphertext(height, &mut rng);

            assert_eq!(
                try_sapling_output_recovery::<TestNetwork>(
                    height,
                    &ovk,
                    &cv,
                    &Fr::random(&mut rng),
                    &epk,
                    &enc_ctext,
                    &out_ctext
                ),
                None
            );
            assert_eq!(
                try_sapling_output_recovery_with_ock::<TestNetwork>(
                    height,
                    &ock.as_bytes(),
                    &Fr::random(&mut rng),
                    &epk,
                    &enc_ctext,
                    &out_ctext
                ),
                None
            );
        }
    }

    #[test]
    fn recovery_with_invalid_epk() {
        let mut rng = OsRng;
        let heights = [
            TestNetwork::activation_height(Sapling).unwrap(),
            TestNetwork::activation_height(Canopy).unwrap(),
        ];

        for &height in heights.iter() {
            let (ovk, ock, _, cv, cmu, _, enc_ciphertext, out_ciphertext) =
                random_enc_ciphertext(height, &mut rng);

            assert_eq!(
                try_sapling_output_recovery::<TestNetwork>(
                    height,
                    &ovk,
                    &cv,
                    &cmu,
                    &edwards::Point::<Bls12, _>::rand(&mut rng, &JUBJUB).mul_by_cofactor(&JUBJUB),
                    &enc_ciphertext,
                    &out_ciphertext
                ),
                None
            );
            assert_eq!(
                try_sapling_output_recovery_with_ock::<TestNetwork>(
                    height,
                    &ock.as_bytes(),
                    &cmu,
                    &edwards::Point::<Bls12, _>::rand(&mut rng, &JUBJUB).mul_by_cofactor(&JUBJUB),
                    &enc_ciphertext,
                    &out_ciphertext
                ),
                None
            );
        }
    }

    #[test]
    fn recovery_with_invalid_enc_tag() {
        let mut rng = OsRng;
        let heights = [
            TestNetwork::activation_height(Sapling).unwrap(),
            TestNetwork::activation_height(Canopy).unwrap(),
        ];

        for &height in heights.iter() {
            let (ovk, ock, _, cv, cmu, epk, mut enc_ciphertext, out_ciphertext) =
                random_enc_ciphertext(height, &mut rng);

            enc_ciphertext[ENC_CIPHERTEXT_SIZE - 1] ^= 0xff;
            assert_eq!(
                try_sapling_output_recovery::<TestNetwork>(
                    height,
                    &ovk,
                    &cv,
                    &cmu,
                    &epk,
                    &enc_ciphertext,
                    &out_ciphertext
                ),
                None
            );
            assert_eq!(
                try_sapling_output_recovery_with_ock::<TestNetwork>(
                    height,
                    &ock.as_bytes(),
                    &cmu,
                    &epk,
                    &enc_ciphertext,
                    &out_ciphertext
                ),
                None
            );
        }
    }

    #[test]
    fn recovery_with_invalid_out_tag() {
        let mut rng = OsRng;
        let heights = [
            TestNetwork::activation_height(Sapling).unwrap(),
            TestNetwork::activation_height(Canopy).unwrap(),
        ];

        for &height in heights.iter() {
            let (ovk, ock, _, cv, cmu, epk, enc_ciphertext, mut out_ciphertext) =
                random_enc_ciphertext(height, &mut rng);

            out_ciphertext[OUT_CIPHERTEXT_SIZE - 1] ^= 0xff;
            assert_eq!(
                try_sapling_output_recovery::<TestNetwork>(
                    height,
                    &ovk,
                    &cv,
                    &cmu,
                    &epk,
                    &enc_ciphertext,
                    &out_ciphertext
                ),
                None
            );
            assert_eq!(
                try_sapling_output_recovery_with_ock::<TestNetwork>(
                    height,
                    &ock.as_bytes(),
                    &cmu,
                    &epk,
                    &enc_ciphertext,
                    &out_ciphertext
                ),
                None
            );
        }
    }

    #[test]
    fn recovery_with_invalid_version_byte() {
        let mut rng = OsRng;
        let canopy_activation_height = TestNetwork::activation_height(Canopy).unwrap();
        let heights = [
            canopy_activation_height - 1,
            canopy_activation_height,
            canopy_activation_height + ZIP212_GRACE_PERIOD,
        ];
        let leadbytes = [0x02, 0x03, 0x01];

        for (&height, &leadbyte) in heights.iter().zip(leadbytes.iter()) {
            let (ovk, ock, _, cv, cmu, epk, mut enc_ciphertext, out_ciphertext) =
                random_enc_ciphertext(height, &mut rng);

            reencrypt_enc_ciphertext(
                &ovk,
                &cv,
                &cmu,
                &epk,
                &mut enc_ciphertext,
                &out_ciphertext,
                |pt| pt[0] = leadbyte,
            );
            assert_eq!(
                try_sapling_output_recovery::<TestNetwork>(
                    height,
                    &ovk,
                    &cv,
                    &cmu,
                    &epk,
                    &enc_ciphertext,
                    &out_ciphertext
                ),
                None
            );
            assert_eq!(
                try_sapling_output_recovery_with_ock::<TestNetwork>(
                    height,
                    &ock.as_bytes(),
                    &cmu,
                    &epk,
                    &enc_ciphertext,
                    &out_ciphertext
                ),
                None
            );
        }
    }

    #[test]
    fn recovery_with_invalid_diversifier() {
        let mut rng = OsRng;
        let heights = [
            TestNetwork::activation_height(Sapling).unwrap(),
            TestNetwork::activation_height(Canopy).unwrap(),
        ];

        for &height in heights.iter() {
            let (ovk, ock, _, cv, cmu, epk, mut enc_ciphertext, out_ciphertext) =
                random_enc_ciphertext(height, &mut rng);

            reencrypt_enc_ciphertext(
                &ovk,
                &cv,
                &cmu,
                &epk,
                &mut enc_ciphertext,
                &out_ciphertext,
                |pt| pt[1..12].copy_from_slice(&find_invalid_diversifier().0),
            );
            assert_eq!(
                try_sapling_output_recovery::<TestNetwork>(
                    height,
                    &ovk,
                    &cv,
                    &cmu,
                    &epk,
                    &enc_ciphertext,
                    &out_ciphertext
                ),
                None
            );
            assert_eq!(
                try_sapling_output_recovery_with_ock::<TestNetwork>(
                    height,
                    &ock.as_bytes(),
                    &cmu,
                    &epk,
                    &enc_ciphertext,
                    &out_ciphertext
                ),
                None
            );
        }
    }

    #[test]
    fn recovery_with_incorrect_diversifier() {
        let mut rng = OsRng;
        let heights = [
            TestNetwork::activation_height(Sapling).unwrap(),
            TestNetwork::activation_height(Canopy).unwrap(),
        ];

        for &height in heights.iter() {
            let (ovk, ock, _, cv, cmu, epk, mut enc_ciphertext, out_ciphertext) =
                random_enc_ciphertext(height, &mut rng);

            reencrypt_enc_ciphertext(
                &ovk,
                &cv,
                &cmu,
                &epk,
                &mut enc_ciphertext,
                &out_ciphertext,
                |pt| pt[1..12].copy_from_slice(&find_valid_diversifier().0),
            );
            assert_eq!(
                try_sapling_output_recovery::<TestNetwork>(
                    height,
                    &ovk,
                    &cv,
                    &cmu,
                    &epk,
                    &enc_ciphertext,
                    &out_ciphertext
                ),
                None
            );
            assert_eq!(
                try_sapling_output_recovery_with_ock::<TestNetwork>(
                    height,
                    &ock.as_bytes(),
                    &cmu,
                    &epk,
                    &enc_ciphertext,
                    &out_ciphertext
                ),
                None
            );
        }
    }

    #[test]
    fn recovery_with_invalid_pk_d() {
        let mut rng = OsRng;
        let heights = [
            TestNetwork::activation_height(Sapling).unwrap(),
            TestNetwork::activation_height(Canopy).unwrap(),
        ];

        for &height in heights.iter() {
            let ivk = Fs::zero();
            let (ovk, ock, _, cv, cmu, epk, enc_ciphertext, out_ciphertext) =
                random_enc_ciphertext_with(height, ivk, &mut rng);

            assert_eq!(
                try_sapling_output_recovery::<TestNetwork>(
                    height,
                    &ovk,
                    &cv,
                    &cmu,
                    &epk,
                    &enc_ciphertext,
                    &out_ciphertext
                ),
                None
            );
            assert_eq!(
                try_sapling_output_recovery_with_ock::<TestNetwork>(
                    height,
                    &ock.as_bytes(),
                    &cmu,
                    &epk,
                    &enc_ciphertext,
                    &out_ciphertext
                ),
                None
            );
        }
    }

    #[test]
    fn test_vectors() {
        let test_vectors = crate::test_vectors::note_encryption::make_test_vectors();

        macro_rules! read_fr {
            ($field:expr) => {{
                Fr::from_repr(FrRepr($field[..].try_into().unwrap())).unwrap()
            }};
        }

        macro_rules! read_fs {
            ($field:expr) => {{
                Fs::from_repr(FsRepr($field[..].try_into().unwrap())).unwrap()
            }};
        }

        macro_rules! read_point {
            ($field:expr) => {
                edwards::Point::<Bls12, _>::read(&$field[..], &JUBJUB).unwrap()
            };
        }

        let height = TestNetwork::activation_height(NetworkUpgrade::Sapling)
            .expect("Should have Sapling activation height");

        for tv in test_vectors {
            //
            // Load the test vector components
            //

            let ivk = read_fs!(tv.ivk);
            let pk_d = read_point!(tv.default_pk_d)
                .as_prime_order(&JUBJUB)
                .unwrap();
            let rcm = read_fs!(tv.rcm);
            let cv = read_point!(tv.cv);
            let cmu = read_fr!(tv.cmu);
            let esk = read_fs!(tv.esk);
            let epk = read_point!(tv.epk).as_prime_order(&JUBJUB).unwrap();

            //
            // Test the individual components
            //

            let shared_secret = sapling_ka_agree(&esk, &pk_d);
            {
                let mut encoded = [0; 32];
                shared_secret
                    .write(&mut encoded[..])
                    .expect("length is not 32 bytes");
                assert_eq!(encoded, tv.shared_secret);
            }

            let k_enc = kdf_sapling(shared_secret, &epk);
            assert_eq!(k_enc.as_bytes(), tv.k_enc);

            let ovk = OutgoingViewingKey(tv.ovk);
            let ock = prf_ock(&ovk, &cv, &cmu, &epk);
            assert_eq!(ock.as_bytes(), tv.ock);

            let to = PaymentAddress::from_parts(Diversifier(tv.default_d), pk_d).unwrap();
            let note = to
                .create_note(tv.v, Rseed::BeforeZip212(rcm), &JUBJUB)
                .unwrap();
            assert_eq!(note.cm(&JUBJUB), cmu);

            //
            // Test decryption
            // (Tested first because it only requires immutable references.)
            //

            match try_sapling_note_decryption::<TestNetwork>(height, &ivk, &epk, &cmu, &tv.c_enc) {
                Some((decrypted_note, decrypted_to, decrypted_memo)) => {
                    assert_eq!(decrypted_note, note);
                    assert_eq!(decrypted_to, to);
                    assert_eq!(&decrypted_memo.0[..], &tv.memo[..]);
                }
                None => panic!("Note decryption failed"),
            }

            match try_sapling_compact_note_decryption::<TestNetwork>(
                height,
                &ivk,
                &epk,
                &cmu,
                &tv.c_enc[..COMPACT_NOTE_SIZE],
            ) {
                Some((decrypted_note, decrypted_to)) => {
                    assert_eq!(decrypted_note, note);
                    assert_eq!(decrypted_to, to);
                }
                None => panic!("Compact note decryption failed"),
            }

            match try_sapling_output_recovery::<TestNetwork>(
                height, &ovk, &cv, &cmu, &epk, &tv.c_enc, &tv.c_out,
            ) {
                Some((decrypted_note, decrypted_to, decrypted_memo)) => {
                    assert_eq!(decrypted_note, note);
                    assert_eq!(decrypted_to, to);
                    assert_eq!(&decrypted_memo.0[..], &tv.memo[..]);
                }
                None => panic!("Output recovery failed"),
            }

            //
            // Test encryption
            //

            let mut ne = SaplingNoteEncryption::new(ovk, note, to, Memo(tv.memo), &mut OsRng);
            // Swap in the ephemeral keypair from the test vectors
            ne.esk = esk;
            ne.epk = epk;

            assert_eq!(&ne.encrypt_note_plaintext()[..], &tv.c_enc[..]);
            assert_eq!(&ne.encrypt_outgoing_plaintext(&cv, &cmu)[..], &tv.c_out[..]);
        }
    }
}
