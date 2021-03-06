//! An implementation of the BLS12-381 pairing-friendly elliptic curve
//! construction.

mod ec;
mod fq;
mod fq12;
mod fq2;
mod fq6;
mod fr;

#[cfg(test)]
mod tests;

pub use self::ec::{
    G1Affine, G1Compressed, G1Uncompressed, G2Affine, G2Compressed, G2Prepared, G2Uncompressed, G1,
    G2,
};
pub use self::fq::{Fq, FqRepr};
pub use self::fq12::Fq12;
pub use self::fq2::Fq2;
pub use self::fq6::Fq6;
pub use self::fr::{Fr, FrRepr};

use super::{Engine, MillerLoopResult, MultiMillerLoop};

use ff::{BitIterator, Field, PrimeField};
use group::{prime::PrimeCurveAffine, Group};
use rand_core::RngCore;
use std::fmt;
use std::iter::Sum;
use std::ops::{Add, AddAssign, Mul, MulAssign, Neg, Sub, SubAssign};
use subtle::{Choice, ConditionallySelectable};

// The BLS parameter x for BLS12-381 is -0xd201000000010000
const BLS_X: u64 = 0xd201000000010000;
const BLS_X_IS_NEGATIVE: bool = true;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Gt(Fq12);

impl fmt::Display for Gt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl ConditionallySelectable for Gt {
    fn conditional_select(a: &Self, b: &Self, choice: Choice) -> Self {
        Gt(Fq12::conditional_select(&a.0, &b.0, choice))
    }
}

impl Neg for Gt {
    type Output = Gt;

    fn neg(self) -> Self::Output {
        let mut ret = self.0;
        ret.conjugate();
        Gt(ret)
    }
}

impl Sum for Gt {
    fn sum<I: Iterator<Item = Self>>(iter: I) -> Self {
        iter.fold(Self::identity(), Add::add)
    }
}

impl<'r> Sum<&'r Gt> for Gt {
    fn sum<I: Iterator<Item = &'r Self>>(iter: I) -> Self {
        iter.fold(Self::identity(), Add::add)
    }
}

impl Add for Gt {
    type Output = Gt;

    fn add(self, rhs: Self) -> Self::Output {
        Gt(self.0 * rhs.0)
    }
}

impl Add<&Gt> for Gt {
    type Output = Gt;

    fn add(self, rhs: &Gt) -> Self::Output {
        Gt(self.0 * rhs.0)
    }
}

impl AddAssign for Gt {
    fn add_assign(&mut self, rhs: Self) {
        self.0 *= rhs.0;
    }
}

impl AddAssign<&Gt> for Gt {
    fn add_assign(&mut self, rhs: &Gt) {
        self.0 *= rhs.0;
    }
}

impl Sub for Gt {
    type Output = Gt;

    fn sub(self, rhs: Self) -> Self::Output {
        self + (-rhs)
    }
}

impl Sub<&Gt> for Gt {
    type Output = Gt;

    fn sub(self, rhs: &Gt) -> Self::Output {
        self + (-*rhs)
    }
}

impl SubAssign for Gt {
    fn sub_assign(&mut self, rhs: Self) {
        *self = *self - rhs;
    }
}

impl SubAssign<&Gt> for Gt {
    fn sub_assign(&mut self, rhs: &Gt) {
        *self = *self - rhs;
    }
}

impl Mul<&Fr> for Gt {
    type Output = Gt;

    fn mul(self, other: &Fr) -> Self::Output {
        let mut acc = Self::identity();

        // This is a simple double-and-add implementation of group element
        // multiplication, moving from most significant to least
        // significant bit of the scalar.
        //
        // We skip the leading bit because it's always unset for Fr
        // elements.
        for bit in other
            .to_repr()
            .as_ref()
            .iter()
            .rev()
            .flat_map(|byte| (0..8).rev().map(move |i| Choice::from((byte >> i) & 1u8)))
            .skip(1)
        {
            acc = acc.double();
            acc = Gt::conditional_select(&acc, &(acc + self), bit);
        }

        acc
    }
}

impl Mul<Fr> for Gt {
    type Output = Gt;

    fn mul(self, other: Fr) -> Self::Output {
        self * &other
    }
}

impl<'r> MulAssign<&'r Fr> for Gt {
    fn mul_assign(&mut self, other: &'r Fr) {
        *self = *self * other
    }
}

impl MulAssign<Fr> for Gt {
    fn mul_assign(&mut self, other: Fr) {
        self.mul_assign(&other);
    }
}

impl Group for Gt {
    type Scalar = Fr;

    fn random<R: RngCore + ?Sized>(_rng: &mut R) -> Self {
        unimplemented!()
    }

    fn identity() -> Self {
        Gt(Fq12::one())
    }

    fn generator() -> Self {
        unimplemented!()
    }

    fn is_identity(&self) -> Choice {
        Choice::from(if self.0 == Fq12::one() { 1 } else { 0 })
    }

    #[must_use]
    fn double(&self) -> Self {
        Gt(self.0.square())
    }
}

#[derive(Clone, Debug)]
pub struct Bls12;

impl Engine for Bls12 {
    type Fr = Fr;
    type G1 = G1;
    type G1Affine = G1Affine;
    type G2 = G2;
    type G2Affine = G2Affine;
    type Gt = Gt;

    fn pairing(p: &Self::G1Affine, q: &Self::G2Affine) -> Self::Gt {
        Self::multi_miller_loop(&[(p, &(*q).into())]).final_exponentiation()
    }
}

impl MultiMillerLoop for Bls12 {
    type G2Prepared = G2Prepared;
    type Result = Fq12;

    fn multi_miller_loop(terms: &[(&Self::G1Affine, &Self::G2Prepared)]) -> Self::Result {
        let mut pairs = vec![];
        for &(p, q) in terms {
            if !bool::from(p.is_identity()) && !q.is_identity() {
                pairs.push((p, q.coeffs.iter()));
            }
        }

        // Twisting isomorphism from E to E'
        fn ell(f: &mut Fq12, coeffs: &(Fq2, Fq2, Fq2), p: &G1Affine) {
            let mut c0 = coeffs.0;
            let mut c1 = coeffs.1;

            c0.c0.mul_assign(&p.y);
            c0.c1.mul_assign(&p.y);

            c1.c0.mul_assign(&p.x);
            c1.c1.mul_assign(&p.x);

            // Sparse multiplication in Fq12
            f.mul_by_014(&coeffs.2, &c1, &c0);
        }

        let mut f = Fq12::one();

        let mut found_one = false;
        for i in BitIterator::<u64, _>::new(&[BLS_X >> 1]) {
            if !found_one {
                found_one = i;
                continue;
            }

            for &mut (p, ref mut coeffs) in &mut pairs {
                ell(&mut f, coeffs.next().unwrap(), p);
            }

            if i {
                for &mut (p, ref mut coeffs) in &mut pairs {
                    ell(&mut f, coeffs.next().unwrap(), p);
                }
            }

            f = f.square();
        }

        for &mut (p, ref mut coeffs) in &mut pairs {
            ell(&mut f, coeffs.next().unwrap(), p);
        }

        if BLS_X_IS_NEGATIVE {
            f.conjugate();
        }

        f
    }
}

impl MillerLoopResult for Fq12 {
    type Gt = Gt;

    fn final_exponentiation(&self) -> Gt {
        let mut f1 = *self;
        f1.conjugate();

        self.invert()
            .map(|mut f2| {
                let mut r = f1;
                r.mul_assign(&f2);
                f2 = r;
                r.frobenius_map(2);
                r.mul_assign(&f2);

                fn exp_by_x(f: &mut Fq12, x: u64) {
                    *f = f.pow_vartime(&[x]);
                    if BLS_X_IS_NEGATIVE {
                        f.conjugate();
                    }
                }

                let mut x = BLS_X;
                let y0 = r.square();
                let mut y1 = y0;
                exp_by_x(&mut y1, x);
                x >>= 1;
                let mut y2 = y1;
                exp_by_x(&mut y2, x);
                x <<= 1;
                let mut y3 = r;
                y3.conjugate();
                y1.mul_assign(&y3);
                y1.conjugate();
                y1.mul_assign(&y2);
                y2 = y1;
                exp_by_x(&mut y2, x);
                y3 = y2;
                exp_by_x(&mut y3, x);
                y1.conjugate();
                y3.mul_assign(&y1);
                y1.conjugate();
                y1.frobenius_map(3);
                y2.frobenius_map(2);
                y1.mul_assign(&y2);
                y2 = y3;
                exp_by_x(&mut y2, x);
                y2.mul_assign(&y0);
                y2.mul_assign(&r);
                y1.mul_assign(&y2);
                y2 = y3;
                y2.frobenius_map(1);
                y1.mul_assign(&y2);

                Gt(y1)
            })
            // self must be nonzero.
            .unwrap()
    }
}

impl G2Prepared {
    pub fn is_identity(&self) -> bool {
        self.infinity
    }

    pub fn from_affine(q: G2Affine) -> Self {
        if q.is_identity().into() {
            return G2Prepared {
                coeffs: vec![],
                infinity: true,
            };
        }

        fn doubling_step(r: &mut G2) -> (Fq2, Fq2, Fq2) {
            // Adaptation of Algorithm 26, https://eprint.iacr.org/2010/354.pdf
            let mut tmp0 = r.x.square();

            let mut tmp1 = r.y.square();

            let mut tmp2 = tmp1.square();

            let mut tmp3 = tmp1;
            tmp3.add_assign(&r.x);
            tmp3 = tmp3.square();
            tmp3.sub_assign(&tmp0);
            tmp3.sub_assign(&tmp2);
            tmp3 = tmp3.double();

            let mut tmp4 = tmp0.double();
            tmp4.add_assign(&tmp0);

            let mut tmp6 = r.x;
            tmp6.add_assign(&tmp4);

            let tmp5 = tmp4.square();

            let zsquared = r.z.square();

            r.x = tmp5;
            r.x.sub_assign(&tmp3);
            r.x.sub_assign(&tmp3);

            r.z.add_assign(&r.y);
            r.z = r.z.square();
            r.z.sub_assign(&tmp1);
            r.z.sub_assign(&zsquared);

            r.y = tmp3;
            r.y.sub_assign(&r.x);
            r.y.mul_assign(&tmp4);

            tmp2 = tmp2.double().double().double();

            r.y.sub_assign(&tmp2);

            tmp3 = tmp4;
            tmp3.mul_assign(&zsquared);
            tmp3 = tmp3.double().neg();

            tmp6 = tmp6.square();
            tmp6.sub_assign(&tmp0);
            tmp6.sub_assign(&tmp5);

            tmp1 = tmp1.double().double();

            tmp6.sub_assign(&tmp1);

            tmp0 = r.z;
            tmp0.mul_assign(&zsquared);
            tmp0 = tmp0.double();

            (tmp0, tmp3, tmp6)
        }

        fn addition_step(r: &mut G2, q: &G2Affine) -> (Fq2, Fq2, Fq2) {
            // Adaptation of Algorithm 27, https://eprint.iacr.org/2010/354.pdf
            let zsquared = r.z.square();

            let ysquared = q.y.square();

            let mut t0 = zsquared;
            t0.mul_assign(&q.x);

            let mut t1 = q.y;
            t1.add_assign(&r.z);
            t1 = t1.square();
            t1.sub_assign(&ysquared);
            t1.sub_assign(&zsquared);
            t1.mul_assign(&zsquared);

            let mut t2 = t0;
            t2.sub_assign(&r.x);

            let t3 = t2.square();

            let t4 = t3.double().double();

            let mut t5 = t4;
            t5.mul_assign(&t2);

            let mut t6 = t1;
            t6.sub_assign(&r.y);
            t6.sub_assign(&r.y);

            let mut t9 = t6;
            t9.mul_assign(&q.x);

            let mut t7 = t4;
            t7.mul_assign(&r.x);

            r.x = t6.square();
            r.x.sub_assign(&t5);
            r.x.sub_assign(&t7);
            r.x.sub_assign(&t7);

            r.z.add_assign(&t2);
            r.z = r.z.square();
            r.z.sub_assign(&zsquared);
            r.z.sub_assign(&t3);

            let mut t10 = q.y;
            t10.add_assign(&r.z);

            let mut t8 = t7;
            t8.sub_assign(&r.x);
            t8.mul_assign(&t6);

            t0 = r.y;
            t0.mul_assign(&t5);
            t0 = t0.double();

            r.y = t8;
            r.y.sub_assign(&t0);

            t10 = t10.square();
            t10.sub_assign(&ysquared);

            let ztsquared = r.z.square();

            t10.sub_assign(&ztsquared);

            t9 = t9.double();
            t9.sub_assign(&t10);

            t10 = r.z.double();

            t6 = t6.neg();

            t1 = t6.double();

            (t10, t1, t9)
        }

        let mut coeffs = vec![];
        let mut r: G2 = q.into();

        let mut found_one = false;
        for i in BitIterator::<u64, _>::new([BLS_X >> 1]) {
            if !found_one {
                found_one = i;
                continue;
            }

            coeffs.push(doubling_step(&mut r));

            if i {
                coeffs.push(addition_step(&mut r, &q));
            }
        }

        coeffs.push(doubling_step(&mut r));

        G2Prepared {
            coeffs,
            infinity: false,
        }
    }
}

#[test]
fn bls12_engine_tests() {
    crate::tests::engine::engine_tests::<Bls12>();
}
