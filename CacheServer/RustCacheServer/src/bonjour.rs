use std::{collections::BTreeSet, net::IpAddr, time::Duration};

use mdns_sd::{ServiceDaemon, ServiceInfo};
use url::Url;

use crate::config::{CacheServerOptions, ConfigError};

pub const BONJOUR_SERVICE_TYPE: &str = "_tvos-net-player._tcp.local.";

const UNREGISTER_TIMEOUT: Duration = Duration::from_millis(250);
const HOST_LABEL_MAX_BYTES: usize = 63;

pub struct BonjourAdvertisement {
    daemon: ServiceDaemon,
    fullname: String,
}

impl BonjourAdvertisement {
    pub fn start(options: &CacheServerOptions) -> Result<Option<Self>, Box<dyn std::error::Error>> {
        if !options.bonjour_enabled {
            return Ok(None);
        }
        if !has_discoverable_grpc_listener(options)? || !has_discoverable_media_endpoint(options)? {
            return Ok(None);
        }

        let service_info = build_service_info(options)?;
        let fullname = service_info.get_fullname().to_owned();
        let daemon = ServiceDaemon::new()?;
        daemon.register(service_info)?;
        Ok(Some(Self { daemon, fullname }))
    }
}

impl Drop for BonjourAdvertisement {
    fn drop(&mut self) {
        if let Ok(receiver) = self.daemon.unregister(&self.fullname) {
            let _ = receiver.recv_timeout(UNREGISTER_TIMEOUT);
        }
        let _ = self.daemon.shutdown();
    }
}

pub fn build_service_info(
    options: &CacheServerOptions,
) -> Result<ServiceInfo, Box<dyn std::error::Error>> {
    let grpc_url = parse_listen_url(&options.grpc_listen_url)?;
    let port = grpc_url
        .port_or_known_default()
        .ok_or_else(|| ConfigError::new("missing gRPC listen port for Bonjour"))?;
    let instance_name = service_instance_name(&options.server_name, &options.server_id);
    let host_name = service_host_name(&options.server_id);
    let version = env!("CARGO_PKG_VERSION");
    let properties = [
        ("server_id", options.server_id.as_str()),
        ("server_name", options.server_name.as_str()),
        ("version", version),
    ];

    match advertised_addresses(options)? {
        AdvertisedAddresses::Auto => Ok(ServiceInfo::new(
            BONJOUR_SERVICE_TYPE,
            &instance_name,
            &host_name,
            (),
            port,
            &properties[..],
        )?
        .enable_addr_auto()),
        AdvertisedAddresses::Static(addresses) => Ok(ServiceInfo::new(
            BONJOUR_SERVICE_TYPE,
            &instance_name,
            &host_name,
            addresses,
            port,
            &properties[..],
        )?),
    }
}

fn parse_listen_url(value: &str) -> Result<Url, ConfigError> {
    let url = Url::parse(value)
        .map_err(|error| ConfigError::new(format!("invalid listen URL: {error}")))?;
    if url.scheme() != "http" {
        return Err(ConfigError::new(
            "Bonjour advertisement requires an http gRPC listen URL.",
        ));
    }

    Ok(url)
}

fn has_discoverable_grpc_listener(options: &CacheServerOptions) -> Result<bool, ConfigError> {
    Ok(options
        .grpc_listen_addrs()?
        .into_iter()
        .any(|addr| !addr.ip().is_loopback()))
}

fn has_discoverable_media_endpoint(options: &CacheServerOptions) -> Result<bool, ConfigError> {
    if let Some(public_base_uri) = options.public_media_base_uri.as_deref() {
        return public_media_base_is_discoverable(public_base_uri);
    }

    Ok(options
        .media_listen_addrs()?
        .into_iter()
        .any(|addr| !addr.ip().is_loopback()))
}

fn public_media_base_is_discoverable(value: &str) -> Result<bool, ConfigError> {
    let url = Url::parse(value)
        .map_err(|error| ConfigError::new(format!("invalid public media base URI: {error}")))?;
    let Some(host) = url.host_str() else {
        return Ok(false);
    };
    let host = host.trim_start_matches('[').trim_end_matches(']');
    if host.eq_ignore_ascii_case("localhost") {
        return Ok(false);
    }
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Ok(!ip.is_loopback() && !ip.is_unspecified());
    }

    Ok(true)
}

enum AdvertisedAddresses {
    Auto,
    Static(String),
}

fn advertised_addresses(options: &CacheServerOptions) -> Result<AdvertisedAddresses, ConfigError> {
    let addresses = options.grpc_listen_addrs()?;
    let mut static_addresses = BTreeSet::new();
    for address in addresses {
        let ip = address.ip();
        if ip.is_loopback() {
            continue;
        }
        if ip.is_unspecified() {
            return Ok(AdvertisedAddresses::Auto);
        }
        static_addresses.insert(ip);
    }

    if static_addresses.is_empty() {
        return Ok(AdvertisedAddresses::Auto);
    }

    Ok(AdvertisedAddresses::Static(
        static_addresses
            .into_iter()
            .map(|ip| ip.to_string())
            .collect::<Vec<_>>()
            .join(","),
    ))
}

fn service_instance_name(server_name: &str, server_id: &str) -> String {
    let name = server_name.trim();
    if !name.is_empty() {
        return truncate_utf8(name, HOST_LABEL_MAX_BYTES).to_owned();
    }

    service_id_label(server_id)
}

fn service_host_name(server_id: &str) -> String {
    format!(
        "{}.local.",
        service_host_label(server_id, system_host_name().as_deref())
    )
}

fn service_host_label(server_id: &str, host_name: Option<&str>) -> String {
    let server_label = service_id_label(server_id);
    let Some(host_label) = host_name.and_then(host_label_component) else {
        return server_label;
    };

    let host_label = truncate_utf8(&host_label, 24).trim_matches('-').to_owned();
    if host_label.is_empty() {
        return server_label;
    }

    let max_server_bytes = HOST_LABEL_MAX_BYTES.saturating_sub(host_label.len() + 1);
    let server_prefix = truncate_utf8(&server_label, max_server_bytes)
        .trim_matches('-')
        .to_owned();
    if server_prefix.is_empty() {
        return host_label;
    }

    format!("{server_prefix}-{host_label}")
}

fn service_id_label(server_id: &str) -> String {
    dns_label_component(server_id).unwrap_or_else(|| "tvos-net-player-cache".to_owned())
}

fn host_label_component(host_name: &str) -> Option<String> {
    let first_label = host_name
        .trim()
        .trim_end_matches('.')
        .split('.')
        .next()
        .unwrap_or_default();
    dns_label_component(first_label)
}

fn dns_label_component(value: &str) -> Option<String> {
    let mut label = String::with_capacity(value.len());
    let mut previous_was_dash = false;
    for byte in value.bytes() {
        let lower = byte.to_ascii_lowercase();
        let valid = lower.is_ascii_lowercase() || lower.is_ascii_digit();
        if valid {
            label.push(char::from(lower));
            previous_was_dash = false;
        } else if !previous_was_dash && !label.is_empty() {
            label.push('-');
            previous_was_dash = true;
        }
    }

    let label = label.trim_matches('-');
    if label.is_empty() {
        return None;
    }

    Some(
        truncate_utf8(label, HOST_LABEL_MAX_BYTES)
            .trim_matches('-')
            .to_owned(),
    )
}

fn system_host_name() -> Option<String> {
    let mut buffer = [0u8; 256];
    let result = unsafe { libc::gethostname(buffer.as_mut_ptr().cast(), buffer.len()) };
    if result != 0 {
        return None;
    }

    let length = buffer
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(buffer.len());
    String::from_utf8(buffer[..length].to_vec())
        .ok()
        .and_then(|value| {
            let trimmed = value.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_owned())
        })
}

fn truncate_utf8(value: &str, max_bytes: usize) -> &str {
    if max_bytes == 0 {
        return "";
    }
    if value.len() <= max_bytes {
        return value;
    }

    let mut end = 0;
    for (index, character) in value.char_indices() {
        let next = index + character.len_utf8();
        if next > max_bytes {
            break;
        }
        end = next;
    }

    &value[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_service_info_uses_grpc_port_and_txt_metadata() {
        let options = CacheServerOptions {
            server_id: "living-room-cache".to_owned(),
            server_name: "Living Room Cache".to_owned(),
            grpc_listen_url: "http://0.0.0.0:51051".to_owned(),
            ..CacheServerOptions::default()
        };

        let service = build_service_info(&options).expect("service info should build");

        assert_eq!(BONJOUR_SERVICE_TYPE, service.get_type());
        assert!(
            service
                .get_fullname()
                .starts_with("Living Room Cache._tvos-net-player._tcp.local.")
        );
        assert!(service.get_hostname().starts_with("living-room-cache"));
        assert!(service.get_hostname().ends_with(".local."));
        assert_eq!(51051, service.get_port());
        assert!(service.is_addr_auto());
        assert!(service.get_addresses().is_empty());
        assert_eq!(
            Some("living-room-cache"),
            service.get_property_val_str("server_id")
        );
        assert_eq!(
            Some("Living Room Cache"),
            service.get_property_val_str("server_name")
        );
        assert_eq!(
            Some(env!("CARGO_PKG_VERSION")),
            service.get_property_val_str("version")
        );
    }

    #[test]
    fn specific_lan_grpc_listener_publishes_only_that_address() {
        let options = CacheServerOptions {
            grpc_listen_url: "http://192.168.1.10:51051".to_owned(),
            ..CacheServerOptions::default()
        };

        let service = build_service_info(&options).expect("service info should build");

        assert!(!service.is_addr_auto());
        assert!(
            service
                .get_addresses()
                .contains(&std::net::IpAddr::from([192, 168, 1, 10]))
        );
        assert_eq!(1, service.get_addresses().len());
    }

    #[test]
    fn service_host_label_is_ascii_dns_safe() {
        assert_eq!("living-room-cache", service_id_label("Living Room Cache!"));
        assert_eq!("tvos-net-player-cache", service_id_label("..."));
    }

    #[test]
    fn service_host_label_includes_machine_identity() {
        assert_eq!(
            "default-mac-mini",
            service_host_label("default", Some("Mac mini.local."))
        );
        assert_eq!(
            "living-room-cache-mac-mini",
            service_host_label("Living Room Cache", Some("mac-mini"))
        );
    }

    #[test]
    fn build_service_info_rejects_non_http_grpc_url() {
        let options = CacheServerOptions {
            grpc_listen_url: "https://127.0.0.1:50051".to_owned(),
            ..CacheServerOptions::default()
        };

        assert!(build_service_info(&options).is_err());
    }

    #[test]
    fn loopback_grpc_listeners_are_not_discoverable() {
        for host in ["localhost", "127.0.0.1", "[::1]"] {
            let options = CacheServerOptions {
                grpc_listen_url: format!("http://{host}:50051"),
                ..CacheServerOptions::default()
            };

            assert!(!has_discoverable_grpc_listener(&options).unwrap());
        }
    }

    #[test]
    fn advertisement_start_skips_default_loopback_listener() {
        let advertisement =
            BonjourAdvertisement::start(&CacheServerOptions::default()).expect("start should skip");

        assert!(advertisement.is_none());
    }

    #[test]
    fn advertisement_start_skips_when_media_listener_is_loopback() {
        let options = CacheServerOptions {
            grpc_listen_url: "http://0.0.0.0:50051".to_owned(),
            ..CacheServerOptions::default()
        };

        let advertisement = BonjourAdvertisement::start(&options).expect("start should skip");

        assert!(advertisement.is_none());
    }

    #[test]
    fn wildcard_and_lan_grpc_listeners_are_discoverable() {
        for host in ["0.0.0.0", "[::]", "192.168.1.10"] {
            let options = CacheServerOptions {
                grpc_listen_url: format!("http://{host}:50051"),
                ..CacheServerOptions::default()
            };

            assert!(has_discoverable_grpc_listener(&options).unwrap());
        }
    }

    #[test]
    fn media_endpoint_requires_lan_listener_or_public_base_uri() {
        let options = CacheServerOptions {
            media_listen_url: "http://localhost:8080".to_owned(),
            ..CacheServerOptions::default()
        };
        assert!(!has_discoverable_media_endpoint(&options).unwrap());

        let options = CacheServerOptions {
            media_listen_url: "http://0.0.0.0:8080".to_owned(),
            ..CacheServerOptions::default()
        };
        assert!(has_discoverable_media_endpoint(&options).unwrap());

        let options = CacheServerOptions {
            public_media_base_uri: Some("http://media.example.test:8080".to_owned()),
            ..CacheServerOptions::default()
        };
        assert!(has_discoverable_media_endpoint(&options).unwrap());

        let options = CacheServerOptions {
            public_media_base_uri: Some("http://localhost:8080".to_owned()),
            ..CacheServerOptions::default()
        };
        assert!(!has_discoverable_media_endpoint(&options).unwrap());
    }
}
