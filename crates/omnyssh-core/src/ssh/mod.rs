/// SSH client, session management, SFTP and metrics collection.
///
/// A native russh client powers metrics collection, SFTP, and the
/// multi-session terminal emulator, plus Smart Server Context with service
/// discovery and Auto SSH Key Setup for secure authentication.
pub mod client;
pub mod discovery;
pub mod jump;
pub mod key_setup;
pub mod metrics;
pub mod pool;
pub mod probe;
pub mod pty;
pub mod services;
pub mod session;
pub mod sftp;
