#![warn(missing_docs)]

extern crate futures;
extern crate tokio_io;
extern crate atm_io_utils;

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
