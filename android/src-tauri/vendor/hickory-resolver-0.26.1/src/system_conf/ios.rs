//! iOS DNS configuration through public libresolv APIs (adapted from Anagram).
use crate::config::{NameServerConfig, ResolverConfig, ResolverOpts};
use hickory_proto::{rr::Name, ProtoError};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct DnsServer {
    address: [u8; 16],
    scope_id: u32,
    port: u16,
    family: u8,
}
#[repr(C)]
struct DnsConfig {
    servers: [DnsServer; 3],
    count: u32,
    ndots: u32,
    search_domains: [[u8; 256]; 6],
}
const _: () = assert!(std::mem::size_of::<DnsServer>() == 24);
const _: () = assert!(std::mem::size_of::<DnsConfig>() == 1616);
#[cfg(target_os = "ios")]
unsafe extern "C" {
    fn napstrfy_ios_read_dns(config: *mut DnsConfig) -> std::ffi::c_int;
}

#[cfg(target_os = "ios")]
pub fn read_system_conf() -> Result<(ResolverConfig, ResolverOpts), ProtoError> {
    let mut raw = DnsConfig {
        servers: [DnsServer::default(); 3],
        count: 0,
        ndots: 0,
        search_domains: [[0; 256]; 6],
    };
    // SAFETY: repr(C) mirrors ios.c, which writes within this initialized buffer.
    if unsafe { napstrfy_ios_read_dns(&mut raw) } != 0 {
        return Err("unable to read iOS resolver configuration".into());
    }
    decode_config(raw)
}

fn decode_config(raw: DnsConfig) -> Result<(ResolverConfig, ResolverOpts), ProtoError> {
    let servers: Vec<_> = raw
        .servers
        .iter()
        .take(raw.count.min(3) as usize)
        .filter_map(|server| {
            let ip = match server.family {
                4 => IpAddr::V4(Ipv4Addr::new(
                    server.address[0],
                    server.address[1],
                    server.address[2],
                    server.address[3],
                )),
                // Hickory 0.26 represents nameservers as IpAddr, without a scope ID.
                // Do not silently send scoped link-local DNS to the wrong interface.
                6 if server.scope_id == 0 => IpAddr::V6(Ipv6Addr::from(server.address)),
                _ => return None,
            };
            let mut config = NameServerConfig::udp_and_tcp(ip);
            for connection in &mut config.connections {
                connection.port = if server.port == 0 { 53 } else { server.port };
            }
            Some(config)
        })
        .collect();
    if servers.is_empty() {
        return Err("no supported iOS resolver addresses".into());
    }
    let search = raw
        .search_domains
        .iter()
        .filter_map(|domain| {
            let end = domain.iter().position(|byte| *byte == 0)?;
            if end == 0 {
                return None;
            }
            Name::from_ascii(std::str::from_utf8(&domain[..end]).ok()?).ok()
        })
        .collect();
    let mut options = ResolverOpts::default();
    options.ndots = raw.ndots as usize;
    Ok((ResolverConfig::from_parts(None, search, servers), options))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn keeps_system_addresses_ports_and_search_domains() {
        let mut raw = DnsConfig {
            servers: [DnsServer::default(); 3],
            count: 99,
            ndots: 2,
            search_domains: [[0; 256]; 6],
        };
        raw.servers[0].family = 4;
        raw.servers[0].address[..4].copy_from_slice(&[192, 0, 2, 53]);
        raw.servers[0].port = 5353;
        raw.servers[1].family = 6;
        raw.servers[1].address = "2001:db8::53".parse::<Ipv6Addr>().unwrap().octets();
        raw.search_domains[0][..12].copy_from_slice(b"example.test");
        let (config, opts) = decode_config(raw).unwrap();
        assert_eq!(config.name_servers().len(), 2);
        assert_eq!(config.name_servers()[0].connections[0].port, 5353);
        assert_eq!(config.name_servers()[1].connections[0].port, 53);
        assert_eq!(config.search()[0].to_ascii(), "example.test");
        assert_eq!(opts.ndots, 2);
    }
    #[test]
    fn refuses_empty_or_unrepresentable_scoped_dns() {
        let mut raw = DnsConfig {
            servers: [DnsServer::default(); 3],
            count: 1,
            ndots: 1,
            search_domains: [[0; 256]; 6],
        };
        raw.servers[0].family = 6;
        raw.servers[0].scope_id = 7;
        raw.servers[0].address = "fe80::53".parse::<Ipv6Addr>().unwrap().octets();
        assert!(decode_config(raw).is_err());
    }
}
