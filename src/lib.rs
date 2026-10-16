pub mod config;
pub mod daemon;
pub mod plugin;
#[allow(clippy::result_large_err)] // tonic's generated API returns tonic::Status.
pub mod proto {
    tonic::include_proto!("aapis.bit.v1");
}
