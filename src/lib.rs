#![feature(generators)]
#![feature(generator_trait)]
#![feature(unboxed_closures)]
#![feature(fn_traits)]

use num_bigint::BigUint;
use num_integer::{Integer, ExtendedGcd};
use num_traits::{One, cast::FromPrimitive};
use tokio::stream::{Stream, StreamExt};
use std::ops::{Add, Rem};
use std::convert::identity;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::marker::PhantomData;
use primal_tokio::primes_unbounded;


// Helper stuff

trait HasModInv {
    fn modinv(&self, m: &Self) -> Option<Self> where Self : Sized;
}

impl<T : Integer + Clone> HasModInv for T where for<'a> &'a T : Add<Output = T> + Rem<Output = T> {
    fn modinv(&self, m: &T) -> Option<T> {
        //modinverse(self, m)
        let ExtendedGcd { gcd, x, .. } = self.extended_gcd(m);
        if !gcd.is_one() {
            None
        } else {
            Some(&(&(&x % m) + m) % m) // Deal with negative values properly
        }
    }
}

/*struct GeneratorIterator<G : Generator<Yield = T, Return = ()>, T> {
    generator: G,
}
impl<G: Generator<Yield = T, Return = ()>, T> GeneratorIterator<G, T> {
    fn new(generator: G) -> GeneratorIterator<G, T> {
        GeneratorIterator { generator }
    }
}
impl<G: Generator<Yield = T, Return = ()> + Unpin, T> Iterator for GeneratorIterator<G, T> {
    type Item = T;

    fn next(&mut self) -> Option<T> {
        match Pin::new(&mut self.generator).resume(()) {
            GeneratorState::Yielded(x) => Some(x),
            GeneratorState::Complete(_) => None,
        }
    }
}*/

// end helper stuff

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

struct StreamFromFuture<F: Future> {
    fut: F,
    is_complete: bool,
}
impl<F: Future> Stream for StreamFromFuture<F> where F: Unpin {
    type Item = F::Output;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<F::Output>> {
        if self.is_complete {
            Poll::Ready(None)
        } else {
            match Pin::new(&mut self.fut).poll(cx) {
                Poll::Ready(x) => {
                    self.is_complete = true;
                    Poll::Ready(Some(x))
                },
                Poll::Pending => Poll::Pending,
            }
        }
    }
}
fn stream_from_future<F: Future>(fut: F) -> impl Stream<Item = F::Output> where F: Unpin {
    StreamFromFuture { fut, is_complete: false }
}

struct StreamFromFutureOption<T, F: Future<Output = Option<T>>> {
    fut: F,
    is_complete: bool,
}
impl<T, F: Future<Output = Option<T>>> Stream for StreamFromFutureOption<T, F> where F: Unpin {
    type Item = T;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<T>> {
        if self.is_complete {
            Poll::Ready(None)
        } else {
            match Pin::new(&mut self.fut).poll(cx) {
                Poll::Ready(x) => {
                    self.is_complete = true;
                    Poll::Ready(x)
                },
                Poll::Pending => Poll::Pending,
            }
        }
    }
}
fn stream_from_future_option<T, F: Future<Output = Option<T>>>(fut: F) -> impl Stream<Item = T> where F: Unpin {
    StreamFromFutureOption { fut, is_complete: false }
}

trait StreamExt2: Stream + Sized {
    fn map_while<T, F: FnMut(Self::Item) -> Option<T>>(self, f: F) -> MapWhileStream<T, Self, F> {
        MapWhileStream { orig: self, f }
    }
    
    /// Some(true) => keep, continue
    /// Some(false) => don't keep, continue
    /// None => don't keep, stop
    fn filter_while<F: FnMut(&Self::Item) -> Option<bool>>(self, f: F) -> FilterWhileStream<Self, F> {
        FilterWhileStream { orig: self, f }
    }
}
impl<S: Stream> StreamExt2 for S {}

struct MapWhileStream<T, S: Stream, F: FnMut(S::Item) -> Option<T>> {
    orig: S,
    f: F,
}
impl<T, S: Stream, F: FnMut(S::Item) -> Option<T>> Stream for MapWhileStream<T, S, F> {
    type Item = T;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<T>> {
        match unsafe { self.as_mut().map_unchecked_mut(|x| &mut x.orig) }.poll_next(cx) {
            Poll::Ready(Some(x)) => Poll::Ready((unsafe { self.get_unchecked_mut() }.f)(x)),
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}

struct FilterWhileStream<S: Stream, F: FnMut(&S::Item) -> Option<bool>> {
    orig: S,
    f: F,
}
impl<S: Stream, F: FnMut(&S::Item) -> Option<bool>> Stream for FilterWhileStream<S, F> where S: Unpin, F: Unpin {
    type Item = S::Item;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<S::Item>> {
        match Pin::new(&mut self.orig).poll_next(cx) {
            Poll::Ready(Some(x)) => match (&mut self.f)(&x) {
                Some(true) => Poll::Ready(Some(x)),
                Some(false) => Poll::Pending,
                None => Poll::Ready(None),
            },
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
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

struct WithData<Val, Data> {
    val: Val,
    #[allow(dead_code)]
    data_box: Box<Data>,
}
fn from_data<'a, Val: 'a, Data: 'a>(mut data_box: Box<Data>, make_val: impl FnOnce(&'a mut Data) -> Val) -> WithData<Val, Data> {
    let val = make_val(unsafe { &mut*(&mut*data_box as *mut _) });
    WithData { val, data_box }
}
impl<Val: Future, Data> Future for WithData<Val, Data> {
    type Output = Val::Output;
    
    fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Val::Output> {
        unsafe { self.map_unchecked_mut(|x| &mut x.val) }.poll(cx)
    }
}

fn find_first_prime_factor<N>(n: N) -> impl Future<Output = Option<usize>> + 'static
    where N: Integer + FromPrimitive + Unpin + 'static,
          for<'a> &'a N: Rem<&'a N, Output = N> {
    from_data(
        Box::new(primes_unbounded().filter_while(move |p| Some((&n % &N::from_usize(*p)?).is_zero()))),
        StreamExt::next,
    )
}

// TODO: future_map
// TODO: find_first_prime_factor

fn get_guesses(knowns: &RsaVars) -> impl Stream<Item = (Guess, bool)> {
    use tokio::stream::*;
    
    empty()
        .merge(stream_from_future_option(find_first_prime_factor(knowns.n.clone())).map(|p| (Guess::P(BigUint::from_usize(p).unwrap()), true)))
}

fn check_guess(knowns: &RsaVars, guess: Guess, is_certain: bool) -> Option<Result<BigUint, Guess>> {
    match guess.clone() {
        Guess::M(m) => {
            if m.modpow(&knowns.e, &knowns.n) == knowns.c {
                return Some(Ok(m));
            }
        },
        Guess::D(d) => {
            let m = knowns.c.modpow(&d, &knowns.n);
            return check_guess(knowns, Guess::M(m), is_certain);
        },
        Guess::Tot(tot) => {
            if let Some(d) = knowns.e.modinv(&tot) {
                return check_guess(knowns, Guess::D(d), is_certain);
            }
        },
        Guess::Pq(p, q) => {
            if !p.is_one() && !q.is_one() && p.clone() * q.clone() == knowns.n {
                let tot = BigUint::lcm(&(p - BigUint::one()), &(q - BigUint::one()));
                return check_guess(knowns, Guess::Tot(tot), is_certain);
            }
        },
        Guess::P(p) => {
            return check_guess(knowns, Guess::Pq(p.clone(), knowns.n.clone() / p), is_certain);
        },
        Guess::Q(q) => {
            return check_guess(knowns, Guess::Pq(knowns.n.clone() / q.clone(), q), is_certain);
        },
    }
    if is_certain {
        Some(Err(guess))
    } else {
        None
    }
}

/// ```
/// use rsa_solver::{find_m, RsaVars};
/// assert_eq!(find_m(&RsaVars { n: (143u8).into(), c: (26u8).into(), e: (17u8).into(), ..Default::default() }), Ok((130u8).into()));
#[tokio::main]
pub async fn find_m(knowns: &RsaVars) -> Result<BigUint, Option<Guess>> {
    match get_guesses(knowns).map(|(guess, is_certain)| check_guess(knowns, guess, is_certain)).filter_map(identity).next().await {
        Some(Ok(x)) => Ok(x),
        Some(Err(x)) => Err(Some(x)),
        None => Err(None),
    }
}

/*pub fn create_rsa_vars_c_n_e(c: BigUint, n: BigUint, e: BigUint) -> RsaVars {
    RsaVars { c, n, e, ..Default::default() }
}*/

/// Copied from https://blairsecrsa.clamchowder.repl.co/
#[cfg(test)]
mod tests {
    #[test]
    fn blairsecrsa_1() {
        let knowns = RsaVars { n: 143, c: 26, e: 17 };
        let m = find_m(&knowns);
        assert_eq!(m, 130);
    }
}
