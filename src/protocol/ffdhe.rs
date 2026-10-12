//! Finite-field Diffie–Hellman for the TLS 1.2 `DHE-RSA` cipher suites.
//!
//! rustls implements DHE from end to end, but neither of its bundled crypto
//! providers ships an FFDHE group (*ring* has no modular exponentiation), so a
//! server that offers only `DHE-RSA-*` suites cannot be reached at all: the
//! client finds no `kx_groups` entry matching the parameters in the server's
//! `ServerKeyExchange` and gives up with
//! `PeerMisbehaved::SelectedUnofferedKxGroup`. Sina's mail servers are exactly
//! that — TLS 1.2 only, no ECDHE, and a fixed 2048-bit prime of their own —
//! which is what this module exists for.
//!
//! It adds the missing pieces and nothing else: an FFDHE [`SupportedKxGroup`]
//! (the modular exponentiation comes from `crypto-bigint`: pure Rust, constant
//! time), the two GCM `DHE-RSA` suites built on top of it, and a
//! [`CryptoProvider`] that is the *ring* provider plus those. The record layer,
//! the PRF, the handshake and the certificate path all stay rustls' own code.

use std::sync::OnceLock;

use crypto_bigint::modular::{BoxedMontyForm, BoxedMontyParams};
use crypto_bigint::zeroize::Zeroize;
use crypto_bigint::{BoxedUint, Odd};
use rustls::crypto::ring::cipher_suite;
use rustls::crypto::{
    ActiveKeyExchange, CipherSuiteCommon, CryptoProvider, KeyExchangeAlgorithm, SecureRandom,
    SharedSecret, SupportedKxGroup,
};
use rustls::ffdhe_groups::{self, FfdheGroup};
use rustls::{
    CipherSuite, Error, NamedGroup, PeerMisbehaved, SupportedCipherSuite, Tls12CipherSuite,
};

/// Every group here is 2048-bit, and `crypto-bigint` wants the precision up
/// front: a value whose precision follows its *contents* (which is what
/// `BoxedUint::from_be_slice_vartime` picks) would make the arithmetic
/// variable-time in the secret exponent. Everything is therefore decoded at
/// this width, and the arithmetic never mixes precisions.
const PRECISION: u32 = 2048;

/// The 2048-bit modulus Sina's mail servers send in every `ServerKeyExchange`:
/// a safe prime (`p = 2q + 1`, both prime) with `g = 2`, the same on
/// `imap.sina.com`, `imap.sina.cn` and `pop.sina.com`, and stable across
/// connections — so it can simply be pinned here. It is *not* RFC 7919's
/// ffdhe2048, which is why it gets a group code of its own (see [`SINA_GROUP`]).
/// SHA-256 of these 256 bytes:
/// `d97428727b9af2146fcd47b383e010ec2a0601268d3075fcd9d4ce868f9c3e24`.
const SINA_DHE2048_P_HEX: &str = "
    9bd9cc566101bf4cc25082fb0ccc080e
    ad733ad8019ceb201bdcd195ac655785
    c945d668a5caa8b6ed8fa7ed07f97e85
    b19986f0675951481cbec90f87e9b365
    0f5095fde5f32db764eadb6c232a0c4d
    3325e86642e01cdf942d4538aefa8912
    338f28f8b28571157da1a4f49b8fa3f8
    c6e0d0512f5d5f480f61a31862327470
    475c38a0f9b6551561a4ba355718354b
    99ad85ae2d771bd824040f99aff15546
    e0aa7896bb261f4fb3718f201321afce
    8fae11b9c09d483261348ed2b4d3be16
    fe8a4b186302525d00147c37e7d0896d
    297bc96f6f2f9701eac7edc45da3601b
    205a7117ee8b69ca02b7f365695c585c
    248e26f92293b69c44ce4f77c0865db3
";

/// Sina's group is not an IANA named group, so it is advertised under a code
/// point of our own: `Unknown` is rustls' documented answer for a key exchange
/// the `NamedGroup` enum has no name for, and 0x01ff is unassigned while still
/// sitting in the 0x0100..0x0200 range rustls classifies as finite-field DH.
/// Servers ignore groups they do not know, so this costs nothing on the wire.
const SINA_GROUP: NamedGroup = NamedGroup::Unknown(0x01ff);

/// The pinned prime, decoded at compile time.
static SINA_DHE2048_P: [u8; 256] = hex_bytes(SINA_DHE2048_P_HEX);

/// One finite-field DH group: its modulus, its generator, and the name it is
/// advertised under.
#[derive(Debug)]
struct FfdheKxGroup {
    name: NamedGroup,
    p: &'static [u8],
    g: &'static [u8],
}

/// The groups Kimo offers. RFC 7919's ffdhe2048 comes first, so that the same
/// code path also serves servers that follow the RFC, followed by Sina's
/// non-standard prime. Both are 2048-bit; nothing here handles another size.
static GROUPS: [FfdheKxGroup; 2] = [
    FfdheKxGroup {
        name: NamedGroup::FFDHE2048,
        p: ffdhe_groups::FFDHE2048.p,
        g: ffdhe_groups::FFDHE2048.g,
    },
    FfdheKxGroup {
        name: SINA_GROUP,
        p: &SINA_DHE2048_P,
        g: &[2],
    },
];

impl SupportedKxGroup for FfdheKxGroup {
    fn start(&self) -> Result<Box<dyn ActiveKeyExchange>, Error> {
        let params = monty_params(self.p)?;
        let p_minus_2 = params.modulus().as_ref().wrapping_sub(small(2));
        // The private exponent lives in [2, p-2] (RFC 2631 §2.2.1). Sampling
        // and retrying keeps it uniform where reducing a random value would
        // bias it towards small exponents; only the retry count varies, and a
        // rejected candidate is never used for anything.
        let x = loop {
            let mut bytes = vec![0u8; self.p.len()];
            secure_random().fill(&mut bytes)?;
            let candidate = uint(&bytes)?;
            let (_, below_two) = candidate.underflowing_sub(small(2));
            let (_, above_p_minus_2) = p_minus_2.underflowing_sub(&candidate);
            if !(below_two | above_p_minus_2).to_bool() {
                break candidate;
            }
        };
        let pub_key = BoxedMontyForm::new(uint(self.g)?, &params)
            .pow(&x)
            .retrieve()
            .to_be_bytes()
            .into_vec();
        Ok(Box::new(FfdheKx {
            name: self.name,
            params,
            p: self.p,
            g: self.g,
            x,
            pub_key,
        }))
    }

    fn ffdhe_group(&self) -> Option<FfdheGroup<'static>> {
        Some(FfdheGroup {
            p: self.p,
            g: self.g,
        })
    }

    fn name(&self) -> NamedGroup {
        self.name
    }
}

/// An FFDHE key exchange in progress: the group's Montgomery parameters, our
/// private exponent, and the public value (`g^x mod p`) already encoded for
/// `ClientKeyExchange`.
struct FfdheKx {
    name: NamedGroup,
    params: BoxedMontyParams,
    p: &'static [u8],
    g: &'static [u8],
    x: BoxedUint,
    pub_key: Vec<u8>,
}

impl ActiveKeyExchange for FfdheKx {
    fn complete(mut self: Box<Self>, peer_pub_key: &[u8]) -> Result<SharedSecret, Error> {
        let peer = uint(peer_pub_key).map_err(|_| invalid_key())?;
        let p_minus_2 = self.params.modulus().as_ref().wrapping_sub(small(2));
        // RFC 2631 §2.2.1 wants 1 < y < p-1, so 0, 1, p-1 and p (which is 0
        // mod p) are all refused rather than turned into a secret. The test is
        // constant time.
        let (_, below_two) = peer.underflowing_sub(small(2));
        let (_, above_p_minus_2) = p_minus_2.underflowing_sub(&peer);
        if (below_two | above_p_minus_2).to_bool() {
            return Err(invalid_key());
        }
        let secret = BoxedMontyForm::new(peer, &self.params)
            .pow(&self.x)
            .retrieve()
            .to_be_bytes();
        self.x.zeroize();
        if secret.iter().all(|byte| *byte == 0) {
            return Err(invalid_key());
        }
        // Full width, as TLS 1.3 wants it; rustls strips the leading zeros
        // again for TLS 1.2 (`complete_for_tls_version`).
        Ok(secret.as_ref().into())
    }

    fn ffdhe_group(&self) -> Option<FfdheGroup<'static>> {
        Some(FfdheGroup {
            p: self.p,
            g: self.g,
        })
    }

    fn group(&self) -> NamedGroup {
        self.name
    }

    fn pub_key(&self) -> &[u8] {
        &self.pub_key
    }
}

/// Kimo's crypto provider: the *ring* one plus [`GROUPS`] and the two
/// `DHE-RSA` suites, appended **last** so that a server offering TLS 1.3 or
/// ECDHE still gets that — DHE is here for servers that offer nothing else, not
/// as a preference. `main.rs` installs it as the process default, which is
/// what `ClientConfig::builder()` picks up.
pub fn crypto_provider() -> CryptoProvider {
    let mut provider = rustls::crypto::ring::default_provider();
    provider.cipher_suites.push(dhe_suite(
        &DHE_RSA_AES_256_GCM_SHA384,
        cipher_suite::TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384,
        CipherSuite::TLS_DHE_RSA_WITH_AES_256_GCM_SHA384,
    ));
    provider.cipher_suites.push(dhe_suite(
        &DHE_RSA_AES_128_GCM_SHA256,
        cipher_suite::TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256,
        CipherSuite::TLS_DHE_RSA_WITH_AES_128_GCM_SHA256,
    ));
    provider.kx_groups.extend(
        GROUPS
            .iter()
            .map(|group| group as &'static dyn SupportedKxGroup),
    );
    provider
}

/// The process-wide CSPRNG for the private exponent: the *ring* provider's,
/// which is the same object [`crypto_provider`] hands rustls.
fn secure_random() -> &'static dyn SecureRandom {
    static RANDOM: OnceLock<&'static dyn SecureRandom> = OnceLock::new();
    *RANDOM.get_or_init(|| rustls::crypto::ring::default_provider().secure_random)
}

/// The two GCM `DHE-RSA` suites, assembled from *ring*'s equivalent ECDHE
/// suites: the AEAD, the PRF and the RSA signature schemes are the very same
/// objects — only the key exchange differs — and rustls keeps those providers
/// private, so lifting them out of a public suite is the way in. Each suite
/// needs a cell of its own because `SupportedCipherSuite` holds a `'static`
/// reference, and the ECDHE templates can only be read at runtime.
static DHE_RSA_AES_256_GCM_SHA384: OnceLock<Tls12CipherSuite> = OnceLock::new();
static DHE_RSA_AES_128_GCM_SHA256: OnceLock<Tls12CipherSuite> = OnceLock::new();

fn dhe_suite(
    cell: &'static OnceLock<Tls12CipherSuite>,
    template: SupportedCipherSuite,
    suite: CipherSuite,
) -> SupportedCipherSuite {
    let SupportedCipherSuite::Tls12(template) = template else {
        unreachable!("the DHE suites are modelled on TLS 1.2 ECDHE suites")
    };
    SupportedCipherSuite::Tls12(cell.get_or_init(|| Tls12CipherSuite {
        common: CipherSuiteCommon {
            suite,
            hash_provider: template.common.hash_provider,
            confidentiality_limit: template.common.confidentiality_limit,
        },
        prf_provider: template.prf_provider,
        kx: KeyExchangeAlgorithm::DHE,
        sign: template.sign,
        aead_alg: template.aead_alg,
    }))
}

/// Decode big-endian bytes at the group's precision. Failing here means the
/// peer sent parameters this module cannot represent, not that a secret is
/// wrong.
fn uint(bytes: &[u8]) -> Result<BoxedUint, Error> {
    BoxedUint::from_be_slice(bytes, PRECISION).map_err(|_| bad_group())
}

/// A small constant at the group's own precision, so that no operation ever
/// mixes precisions.
fn small(value: u8) -> BoxedUint {
    uint(&[value]).expect("a single byte always fits 2048 bits")
}

fn monty_params(p: &[u8]) -> Result<BoxedMontyParams, Error> {
    let odd = Odd::new(uint(p)?).into_option().ok_or_else(bad_group)?;
    Ok(BoxedMontyParams::new(odd))
}

/// The peer's public key is degenerate, or the secret it yields is zero:
/// refused rather than used.
fn invalid_key() -> Error {
    Error::PeerMisbehaved(PeerMisbehaved::InvalidKeyShare)
}

/// A group whose parameters do not fit this module's fixed precision.
fn bad_group() -> Error {
    Error::General("FFDHE: parameters do not fit a 2048-bit group".into())
}

/// Decode a hex literal at compile time, ignoring whitespace and underscores so
/// that a long prime can be laid out in readable rows. Panics (at compile time)
/// on a stray character, an odd number of digits, or the wrong length.
const fn hex_bytes<const N: usize>(hex: &str) -> [u8; N] {
    let src = hex.as_bytes();
    let mut out = [0u8; N];
    let mut read = 0;
    let mut written = 0;
    let mut high: Option<u8> = None;
    while read < src.len() {
        let digit = match src[read] {
            b'0'..=b'9' => src[read] - b'0',
            b'a'..=b'f' => src[read] - b'a' + 10,
            b'A'..=b'F' => src[read] - b'A' + 10,
            b' ' | b'\t' | b'\n' | b'\r' | b'_' => {
                read += 1;
                continue;
            }
            _ => panic!("not a hex digit"),
        };
        read += 1;
        match high {
            None => high = Some(digit),
            Some(first) => {
                out[written] = (first << 4) | digit;
                written += 1;
                high = None;
            }
        }
    }
    assert!(high.is_none(), "odd number of hex digits");
    assert!(written == N, "hex literal has the wrong length");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn group(name: NamedGroup) -> &'static FfdheKxGroup {
        GROUPS
            .iter()
            .find(|group| group.name == name)
            .expect("group is registered")
    }

    #[test]
    fn hex_bytes_ignores_layout_and_case() {
        assert_eq!(hex_bytes::<2>(" 0a\nB_1\t"), [0x0a, 0xb1]);
        assert_eq!(hex_bytes::<3>("9BD9cc"), [0x9b, 0xd9, 0xcc]);
    }

    #[test]
    fn pinned_prime_is_the_documented_2048_bit_odd_modulus() {
        assert_eq!(SINA_DHE2048_P.len(), 256);
        assert_eq!(SINA_DHE2048_P[0], 0x9b);
        assert_eq!(SINA_DHE2048_P[255], 0xb3);
        assert_eq!(SINA_DHE2048_P[255] & 1, 1, "a DH modulus is odd");
        assert_ne!(
            group(SINA_GROUP).p,
            ffdhe_groups::FFDHE2048.p,
            "the pinned prime is Sina's own, not RFC 7919's"
        );
    }

    #[test]
    fn every_group_reports_the_parameters_it_works_in() {
        for group in &GROUPS {
            assert_eq!(group.p.len(), 256, "{:?} is 2048-bit", group.name);
            assert_eq!(group.g, &[2], "{:?} uses generator 2", group.name);
            assert_eq!(
                group.ffdhe_group(),
                Some(FfdheGroup {
                    p: group.p,
                    g: group.g
                }),
                "{:?} must report the parameters the server sent",
                group.name
            );
        }
    }

    #[test]
    fn two_exchanges_agree_on_a_shared_secret() {
        let group = group(SINA_GROUP);
        let client = group.start().expect("key generation");
        let server = group.start().expect("key generation");
        let client_pub = client.pub_key().to_vec();
        let server_pub = server.pub_key().to_vec();
        assert_eq!(client_pub.len(), 256);
        let client_secret = client.complete(&server_pub).expect("shared secret");
        let server_secret = server.complete(&client_pub).expect("shared secret");
        assert_eq!(client_secret.secret_bytes(), server_secret.secret_bytes());
        assert_eq!(
            client_secret.secret_bytes().len(),
            256,
            "the secret is left-padded to the modulus width"
        );
        assert!(client_secret.secret_bytes().iter().any(|byte| *byte != 0));
    }

    #[test]
    fn degenerate_peer_keys_are_refused() {
        let group = group(SINA_GROUP);
        let mut p_minus_1 = group.p.to_vec();
        *p_minus_1.last_mut().expect("non-empty") -= 1; // p is odd, so this is p-1
        for peer in [vec![0u8], vec![1u8], group.p.to_vec(), p_minus_1] {
            let kx = group.start().expect("key generation");
            assert!(
                kx.complete(&peer).is_err(),
                "peer key {peer:02x?} must be refused"
            );
        }
    }

    #[test]
    fn the_provider_adds_dhe_behind_the_existing_suites() {
        let provider = crypto_provider();
        let suites: Vec<CipherSuite> = provider
            .cipher_suites
            .iter()
            .map(SupportedCipherSuite::suite)
            .collect();
        let dhe = suites
            .iter()
            .position(|suite| *suite == CipherSuite::TLS_DHE_RSA_WITH_AES_256_GCM_SHA384)
            .expect("DHE-RSA/AES-256-GCM is offered");
        let ecdhe = suites
            .iter()
            .position(|suite| *suite == CipherSuite::TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384)
            .expect("ECDHE-RSA/AES-256-GCM is offered");
        assert_eq!(suites[0], CipherSuite::TLS13_AES_256_GCM_SHA384);
        assert!(dhe > ecdhe, "DHE must stay the fallback");
        assert!(suites.contains(&CipherSuite::TLS_DHE_RSA_WITH_AES_128_GCM_SHA256));
        assert!(provider.kx_groups.iter().any(|group| group.ffdhe_group()
            == Some(FfdheGroup {
                p: &SINA_DHE2048_P,
                g: &[2]
            })));
    }
}
