// Exercise the actual iOS resolver conversion on development/CI hosts.
mod config {
    pub use hickory_resolver::config::{NameServerConfig, ResolverConfig, ResolverOpts};
}
#[path = "../vendor/hickory-resolver-0.26.1/src/system_conf/ios.rs"]
mod ios_dns;
