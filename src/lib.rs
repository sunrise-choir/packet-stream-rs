#![feature(closure_to_fn_coercion)]

#![warn(missing_docs)]

#[macro_use(try_ready)]
extern crate futures;
extern crate tokio_io;
extern crate atm_io_utils;
extern crate atm_async_utils;
extern crate void;
extern crate multi_producer_sink;
extern crate multi_consumer_stream;

#[cfg(test)]
extern crate partial_io;
#[cfg(test)]
extern crate quickcheck;
#[cfg(test)]
extern crate async_ringbuffer;
#[cfg(test)]
extern crate rand;

mod raw;
mod codec;
pub mod ps;
