#[cfg(not(unix))]
compile_error!("nunc requires a Unix filesystem and POSIX FIFOs");

pub mod cli;
pub mod entries;
pub mod fifo;
mod fs;
mod protocol;
pub mod repo;
