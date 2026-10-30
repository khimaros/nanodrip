//! ec j-pake over p-256, as the sensor runs it: three rounds of schnorr proofs
//! keyed by the pairing code, wire compatible with mbedtls ecjpake hashing.

use p256::elliptic_curve::group::Group;
use p256::elliptic_curve::ops::Reduce;
use p256::elliptic_curve::point::AffineCoordinates;
use p256::elliptic_curve::sec1::{FromEncodedPoint, ToEncodedPoint};
use p256::elliptic_curve::PrimeField;
use p256::{AffinePoint, EncodedPoint, FieldBytes, NonZeroScalar, ProjectivePoint, Scalar, U256};
use rand_core::CryptoRngCore;
use sha2::{Digest, Sha256};

pub const PACKET_SIZE: usize = 160;
pub const KEY_SIZE: usize = 16;
const COORD_SIZE: usize = 32;
const SCALAR_OFFSET: usize = COORD_SIZE * 4;
const CLIENT: &[u8] = b"client";
const SERVER: &[u8] = b"server";

pub type Packet = [u8; PACKET_SIZE];
pub type SharedKey = [u8; KEY_SIZE];

fn reduce(bytes: &FieldBytes) -> Scalar {
    <Scalar as Reduce<U256>>::reduce_bytes(bytes)
}

/// interprets the code bytes as a big endian integer.
fn password_scalar(code: &[u8]) -> Scalar {
    let mut wide = FieldBytes::default();
    let start = wide.len() - code.len();
    wide[start..].copy_from_slice(code);
    reduce(&wide)
}

fn write_point(digest: &mut Sha256, point: &ProjectivePoint) {
    let encoded = point.to_affine().to_encoded_point(false);
    digest.update((encoded.len() as u32).to_be_bytes());
    digest.update(encoded.as_bytes());
}

fn challenge(
    g: &ProjectivePoint,
    gv: &ProjectivePoint,
    gx: &ProjectivePoint,
    party: &[u8],
) -> Scalar {
    let mut digest = Sha256::new();
    write_point(&mut digest, g);
    write_point(&mut digest, gv);
    write_point(&mut digest, gx);
    digest.update((party.len() as u32).to_be_bytes());
    digest.update(party);
    reduce(&digest.finalize())
}

fn write_coords(out: &mut [u8], point: &ProjectivePoint) {
    let encoded = point.to_affine().to_encoded_point(false);
    out[..COORD_SIZE * 2].copy_from_slice(&encoded.as_bytes()[1..]);
}

fn field_bytes(bytes: &[u8]) -> FieldBytes {
    let mut out = FieldBytes::default();
    out.copy_from_slice(&bytes[..COORD_SIZE]);
    out
}

fn read_point(bytes: &[u8]) -> Option<ProjectivePoint> {
    let encoded = EncodedPoint::from_affine_coordinates(
        &field_bytes(bytes),
        &field_bytes(&bytes[COORD_SIZE..]),
        false,
    );
    let point: Option<AffinePoint> = AffinePoint::from_encoded_point(&encoded).into();
    point.map(ProjectivePoint::from)
}

fn prove(rng: &mut impl CryptoRngCore, g: &ProjectivePoint, x: &Scalar) -> Packet {
    let v = NonZeroScalar::random(rng);
    let public = *g * x;
    let gv = *g * *v;
    let r = *v - challenge(g, &gv, &public, CLIENT) * x;
    let mut packet = [0u8; PACKET_SIZE];
    write_coords(&mut packet, &public);
    write_coords(&mut packet[COORD_SIZE * 2..], &gv);
    packet[SCALAR_OFFSET..].copy_from_slice(&r.to_repr());
    packet
}

/// returns the peer public point when its schnorr proof holds.
fn verify(g: &ProjectivePoint, packet: &[u8]) -> Option<ProjectivePoint> {
    let public = read_point(packet)?;
    let gv = read_point(&packet[COORD_SIZE * 2..])?;
    let r: Option<Scalar> = Scalar::from_repr(field_bytes(&packet[SCALAR_OFFSET..])).into();
    let expected = *g * r? + public * challenge(g, &gv, &public, SERVER);
    (expected == gv && !bool::from(public.is_identity())).then_some(public)
}

pub struct Jpake {
    x1: Scalar,
    x2: Scalar,
    x2s: Scalar,
    peer: [ProjectivePoint; 3],
}

impl Jpake {
    pub fn new(rng: &mut impl CryptoRngCore, code: &[u8]) -> Self {
        let x1 = *NonZeroScalar::random(&mut *rng);
        let x2 = *NonZeroScalar::random(&mut *rng);
        Self {
            x1,
            x2,
            x2s: x2 * password_scalar(code),
            peer: [ProjectivePoint::IDENTITY; 3],
        }
    }

    fn public(x: &Scalar) -> ProjectivePoint {
        ProjectivePoint::GENERATOR * x
    }

    /// validates the sensor packet for a zero based round, recording its point.
    pub fn accept(&mut self, round: usize, packet: &[u8]) -> bool {
        let g = match round {
            0 | 1 => ProjectivePoint::GENERATOR,
            _ => Self::public(&self.x1) + Self::public(&self.x2) + self.peer[0],
        };
        match verify(&g, packet) {
            Some(public) => {
                self.peer[round] = public;
                true
            }
            None => false,
        }
    }

    /// builds our packet for a zero based round.
    pub fn round(&self, rng: &mut impl CryptoRngCore, round: usize) -> Packet {
        match round {
            0 => prove(rng, &ProjectivePoint::GENERATOR, &self.x1),
            1 => prove(rng, &ProjectivePoint::GENERATOR, &self.x2),
            _ => {
                let g = Self::public(&self.x1) + self.peer[0] + self.peer[1];
                prove(rng, &g, &self.x2s)
            }
        }
    }

    pub fn shared_key(&self) -> SharedKey {
        let k = (self.peer[2] - self.peer[1] * self.x2s) * self.x2;
        let digest = Sha256::digest(k.to_affine().x());
        let mut key = [0u8; KEY_SIZE];
        key.copy_from_slice(&digest[..KEY_SIZE]);
        key
    }
}
