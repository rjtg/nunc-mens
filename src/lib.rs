#[cfg(not(unix))]
compile_error!("nunc requires a Unix filesystem and POSIX FIFOs");

pub mod cli;
mod documents;
pub mod entries;
pub mod fifo;
mod fs;
mod index;
mod protocol;
pub mod repo;
mod vectors;
