#![feature(generators)]
#![feature(generator_trait)]
#![feature(unboxed_closures)]
#![feature(fn_traits)]
#![feature(type_ascription)]

use crate::utils::HasModInv;
use num_bigint::{BigInt, BigUint};
use num_integer::Integer;
use num_traits::{
    cast::{FromPrimitive, ToPrimitive},
    CheckedSub, One, Zero,
};
use primal_tokio::primes_unbounded;
use std::{convert::identity, iter::successors, string::FromUtf8Error};
use tokio::stream::{self, Stream, StreamExt};

mod utils {
    use num_bigint::{BigInt, BigUint, Sign};
    use num_integer::{ExtendedGcd, Integer};
    use num_traits::One;
    use std::{
        future::Future,
        marker::PhantomData,
        pin::Pin,
        task::{Context, Poll},
    };
    use tokio::stream::Stream;

    pub trait HasModInv {
        fn modinv(&self, m: &Self) -> Option<Self>
        where
            Self: Sized;
    }

    impl HasModInv for BigInt {
        fn modinv(&self, m: &Self) -> Option<Self> {
            let ExtendedGcd { gcd, x, .. } = self.extended_gcd(m);
            if gcd.is_one() {
                Some(&(&(&x % m) + m) % m) // Deal with negative values properly
            } else {
                None
            }
        }
    }

    impl HasModInv for BigUint {
        fn modinv(&self, m: &Self) -> Option<Self> {
            BigInt::from_biguint(Sign::Plus, self.clone())
                .modinv(&BigInt::from_biguint(Sign::Plus, m.clone()))?
                .to_biguint()
        }
    }

    pub struct StreamFromFutureOption<T, F: Future<Output = Option<T>>> {
        fut: F,
        is_complete: bool,
    }
    impl<T, F: Future<Output = Option<T>>> Stream for StreamFromFutureOption<T, F>
    where
        F: Unpin,
    {
        type Item = T;

        fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<T>> {
            if self.is_complete {
                Poll::Ready(None)
            } else {
                match Pin::new(&mut self.fut).poll(cx) {
                    Poll::Ready(x) => {
                        self.is_complete = true;
                        Poll::Ready(x)
                    }
                    Poll::Pending => Poll::Pending,
                }
            }
        }
    }

    pub trait StreamExt2: Stream + Sized {
        fn map_while<T, F: FnMut(Self::Item) -> Option<T>>(
            self,
            f: F,
        ) -> MapWhileStream<T, Self, F> {
            MapWhileStream { orig: self, f }
        }

        /// Some(true) => keep, continue
        /// Some(false) => don't keep, continue
        /// None => don't keep, stop
        fn filter_while<F: FnMut(&Self::Item) -> Option<bool>>(
            self,
            f: F,
        ) -> FilterWhileStream<Self, F> {
            FilterWhileStream { orig: self, f }
        }
    }
    impl<S: Stream> StreamExt2 for S {}

    pub struct MapWhileStream<T, S: Stream, F: FnMut(S::Item) -> Option<T>> {
        orig: S,
        f: F,
    }
    impl<T, S: Stream, F: FnMut(S::Item) -> Option<T>> Stream for MapWhileStream<T, S, F>
    where
        S: Unpin,
        F: Unpin,
    {
        type Item = T;

        fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<T>> {
            loop {
                match Pin::new(&mut self.orig).poll_next(cx) {
                    Poll::Ready(Some(x)) => return Poll::Ready((&mut self.f)(x)),
                    Poll::Ready(None) => return Poll::Ready(None),
                    Poll::Pending => {}
                }
            }
        }
    }

    pub struct FilterWhileStream<S: Stream, F: FnMut(&S::Item) -> Option<bool>> {
        orig: S,
        f: F,
    }
    impl<S: Stream, F: FnMut(&S::Item) -> Option<bool>> Stream for FilterWhileStream<S, F>
    where
        S: Unpin,
        F: Unpin,
    {
        type Item = S::Item;

        fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<S::Item>> {
            loop {
                match Pin::new(&mut self.orig).poll_next(cx) {
                    Poll::Ready(Some(x)) => match (&mut self.f)(&x) {
                        Some(true) => return Poll::Ready(Some(x)),
                        Some(false) => {}
                        None => return Poll::Ready(None),
                    },
                    Poll::Ready(None) => return Poll::Ready(None),
                    Poll::Pending => return Poll::Pending,
                }
            }
        }
    }

    struct FnMutWithData<State, In, Out, F: Fn(&mut State, In) -> Out> {
        state: State,
        f: F,
        phantom: PhantomData<fn(&mut State, In) -> Out>,
    }
    impl<State, In, Out, F: Fn(&mut State, In) -> Out> FnOnce<In> for FnMutWithData<State, In, Out, F> {
        type Output = Out;

        extern "rust-call" fn call_once(mut self, args: In) -> Out {
            (self.f)(&mut self.state, args)
        }
    }
    impl<State, In, Out, F: Fn(&mut State, In) -> Out> FnMut<In> for FnMutWithData<State, In, Out, F> {
        extern "rust-call" fn call_mut(&mut self, args: In) -> Out {
            (self.f)(&mut self.state, args)
        }
    }
}

// TODO: figure out what to do with d, p, q, & tot
// Maybe allow custom hints in general?

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RsaVars {
    pub c: BigUint,
    //pub d: Option<BigUint>,
    pub e: BigUint,
    pub n: BigUint,
    //pub p: Option<BigUint>,
    //pub q: Option<BigUint>,
    //pub tot: Option<BigUint>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Guess {
    M(BigUint),
    D(BigUint),
    Tot(BigUint),
    Pq(BigUint, BigUint),
    P(BigUint),
    Q(BigUint),
}

#[allow(clippy::cast_sign_loss)]
#[inline]
const fn isize_neg_as_usize(a: isize) -> usize {
    a.wrapping_neg() as usize
}

#[allow(clippy::cast_sign_loss)]
#[inline]
fn add_biguint_isize_checked(a: &BigUint, b: isize) -> Option<BigUint> {
    if b < 0 {
        a.checked_sub(&BigUint::from_usize(isize_neg_as_usize(b))?)
    } else {
        Some(a + (b as usize))
    }
}

fn get_guesses(knowns: &RsaVars) -> impl Stream<Item = (Guess, bool)> + 'static {
    let c1 = knowns.c.clone();
    let n1 = knowns.n.clone();
    let n2 = knowns.n.clone();
    let e_u32_maybe = knowns.e.to_u32();
    let approx_sqrt_n = knowns.n.sqrt();

    stream::empty()
        .merge(
            stream::empty()
                .merge(primes_unbounded().map(BigUint::from))
                .merge(
                    stream::iter((0_isize..=isize::MAX).flat_map(|i| vec![i, -i - 1]))
                        .filter_map(move |i| Some(add_biguint_isize_checked(&approx_sqrt_n, i)?)),
                )
                .map(|p| (Guess::P(p), false)),
        )
        .merge(
            if let Some(e_u32) = e_u32_maybe {
                Box::new(
                    stream::iter(successors(Some(BigUint::zero()), |i| Some(i + 1_u8)))
                        .map(move |i| (&c1 + &n1 * i).nth_root(e_u32))
                        .take_while(move |m| m < &n2)
                        .map(|m| (Guess::M(m), false)),
                )
            } else {
                Box::new(stream::empty())
            }: Box<dyn Stream<Item = _> + Unpin>,
        )
}

fn check_guess(knowns: &RsaVars, guess: Guess, is_certain: bool) -> Option<Result<BigUint, Guess>> {
    match guess.clone() {
        Guess::M(m) => {
            if m.modpow(&knowns.e, &knowns.n) == knowns.c {
                return Some(Ok(m));
            }
        }
        Guess::D(d) => {
            let m = knowns.c.modpow(&d, &knowns.n);
            return check_guess(knowns, Guess::M(m), is_certain);
        }
        Guess::Tot(tot) => {
            if let Some(d_signed) = BigInt::from(knowns.e.clone()).modinv(&BigInt::from(tot)) {
                if let Some(d) = d_signed.to_biguint() {
                    return check_guess(knowns, Guess::D(d), is_certain);
                }
            }
        }
        Guess::Pq(p, q) => {
            if !p.is_one() && !q.is_one() && p.clone() * q.clone() == knowns.n {
                let tot = BigUint::lcm(&(p - BigUint::one()), &(q - BigUint::one()));
                return check_guess(knowns, Guess::Tot(tot), is_certain);
            }
        }
        Guess::P(p) => {
            return check_guess(
                knowns,
                Guess::Pq(p.clone(), knowns.n.clone() / p),
                is_certain,
            );
        }
        Guess::Q(q) => {
            return check_guess(
                knowns,
                Guess::Pq(knowns.n.clone() / q.clone(), q),
                is_certain,
            );
        }
    }
    if is_certain {
        Some(Err(guess))
    } else {
        None
    }
}

#[tokio::main]
pub async fn find_m(
    knowns: &RsaVars,
    extra_guesses: impl Stream<Item = (Guess, bool)> + Unpin,
) -> Result<BigUint, Option<Guess>> {
    match get_guesses(knowns)
        .merge(extra_guesses)
        .map(|(guess, is_certain)| check_guess(knowns, guess, is_certain))
        .filter_map(identity)
        .next()
        .await
    {
        Some(Ok(x)) => Ok(x),
        Some(Err(x)) => Err(Some(x)),
        None => Err(None),
    }
}

/// # Errors
/// Errors if UTF-8 decoding fails.
pub fn integer_to_text(x: &BigUint) -> Result<String, FromUtf8Error> {
    String::from_utf8(x.to_bytes_be())
}

#[macro_export]
macro_rules! biguint_base_10 {
    ($bytestring:literal) => {
        BigUint::parse_bytes($bytestring, 10).unwrap()
    };
}

/// Copied from https://blairsecrsa.clamchowder.repl.co/
#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils::StreamExt2;
    use num_traits::Zero;
    use std::iter::{once, repeat};
    use tokio::stream;

    #[tokio::test]
    async fn test_filter_while_some_true() {
        assert_eq!(
            stream::iter(repeat(1))
                .filter_while(move |_x| Some(true))
                .next()
                .await,
            Some(1),
        );
    }

    #[tokio::test]
    async fn test_filter_while_some_false() {
        assert_eq!(
            stream::iter(once(1).chain(once(2)).chain(repeat(3)))
                .filter_while(move |x| Some(*x == 2))
                .next()
                .await,
            Some(2),
        );
    }

    #[tokio::test]
    async fn test_filter_while_none() {
        assert_eq!(
            stream::iter(once(1).chain(once(2)).chain(repeat(3)))
                .filter_while(move |x| match *x {
                    1 => Some(false),
                    2 => None,
                    3 => Some(true),
                    _ => panic!(),
                })
                .next()
                .await,
            None,
        );
    }

    #[tokio::test]
    async fn test_first_prime_factor_2_manual() {
        assert_eq!(
            primes_unbounded()
                .filter_while(move |p| Some((&BigUint::from(2u8) % &BigUint::from(*p)).is_zero()))
                .next()
                .await,
            Some(2)
        );
    }

    #[tokio::test]
    async fn test_first_primes_unbounded() {
        assert_eq!(
            primes_unbounded()
                .filter_while(move |_p| Some(true))
                .next()
                .await,
            Some(2)
        );
    }

    #[test]
    fn blairsecrsa_1() {
        let knowns = RsaVars {
            n: BigUint::from(143u8),
            c: BigUint::from(26u8),
            e: BigUint::from(17u8),
        };
        let m = find_m(&knowns, stream::empty());
        assert_eq!(m, Ok(BigUint::from(130u8)));
    }

    #[test]
    fn blairsecrsa_2() {
        let knowns = RsaVars {
            n: biguint_base_10!(b"7189802717771567255220150620784419218541052212701457717541277400875935717509112424332675475828865427129929478478705214406863743117810353034221864597059029"),
            c: biguint_base_10!(b"6751783441286199006649089194985094993886902223296203844561033180464677568123886846622027779778424322403187862229955233916571566534078605876657505484780416"),
            e: BigUint::from(65537u32),
        };
        let m = find_m(&knowns, stream::once((Guess::D(biguint_base_10!(b"60521148348322035935880237003007023038820012166261869999800693239186381293403217600217141646114073805127564478574625302642602746961775824519317916708573")), true)));
        assert_eq!(
            integer_to_text(&m.unwrap()).unwrap(),
            "math_team_moved_to_room_314"
        );
    }

    #[test]
    fn blairsecrsa_3() {
        let knowns = RsaVars {
            n: biguint_base_10!(b"14797548547156632301969225821934492731102670684667903621151016093295053040114096328625926272798085632301613712041652489095840382483306442874016530106414090585452689093972987761198773025427792415934797604114686665980378348144178690723693394148357070361961816231025853178162613437031794991120929868540933346797435026495032392347502053866152046793252790268130353779532453717072637972954909589584203377069165031675713590802461859140674796815146481680286887646674083998943581947179319792591983283853613837503874609657218599157198238900748459605338558300914906522418271650774235413036583240805079020036020551892796083524871"),
            c: biguint_base_10!(b"4719076732212728094375303980830350595206208731351841162735845737519200854000512597594943849375922531193477166368671188265816054600777424637756103377599837752045594469968862965796572293632"),
            e: BigUint::from(3u8),
        };
        let m = find_m(&knowns, stream::empty());
        assert_eq!(
            integer_to_text(&m.unwrap()).unwrap(),
            "happy late birthday to kmh"
        );
    }

    #[test]
    fn blairsecrsa_4() {
        let knowns = RsaVars {
            n: biguint_base_10!(b"69978962132462700542314544691772639070611340838796818854806308882491349635729665000740158256677514009581276654324098401643583698039286173305307386463858790481569491558519134818744276395633014587167181270948385909932023948295336629660458744242415640711595427718601555108035280623072770272466690875453227602019"),
            c: biguint_base_10!(b"69266611370829490409492107191591011965380547104132610658405993536354837660890492057218061779222565998572564945825152853076598050713201442599297306242754142186965496804589032543650867598357898675934350067192085061215624008917428508725240979881417359956262115203611248687828748979793921532789960399322928138049"),
            e: biguint_base_10!(b"69266611370829490409492107191591011965380547104132610658405993536354837660890492057218061779222565998572564945825152853076598050713201442599297306242754142186965496804589032543650867598357898675934350067192085061215624008917428508725240979881417359956262115203611248687828748979793921532789960399322928138049"),
        };
        let m = find_m(&knowns, stream::empty());
        panic!(integer_to_text(&m.unwrap()).unwrap());
    }
}
