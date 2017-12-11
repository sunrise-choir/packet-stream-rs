#![warn(missing_docs)]
#![feature(conservative_impl_trait)]

#[macro_use]
extern crate futures;
extern crate tokio_io;
#[macro_use]
extern crate atm_io_utils;

#[cfg(test)]
extern crate partial_io;
#[cfg(test)]
#[macro_use]
extern crate quickcheck;
#[cfg(test)]
extern crate async_ringbuffer;
#[cfg(test)]
extern crate rand;

mod raw;
mod codec;
