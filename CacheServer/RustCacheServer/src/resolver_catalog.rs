use std::collections::HashSet;

use serde::Deserialize;
use url::{Host, Url};

// Imported from Joey-Project/BBDown-rust at revision 72b0c1ed5313df07ec5441cc4654afc1521d5165.
const RESOLVER_CATALOG_JSON: &str = include_str!("resolver_catalog.json");

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Region {
    All,
    Cn,
    Hk,
    Tw,
    Th,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub(crate) struct ResolverEntry {
    pub(crate) name: String,
    pub(crate) host: String,
    pub(crate) regions: Vec<Region>,
}

fn load_catalog(json: &str) -> Result<Vec<ResolverEntry>, String> {
    let entries: Vec<ResolverEntry> =
        serde_json::from_str(json).map_err(|_| "invalid resolver catalog JSON".to_owned())?;
    let mut names = HashSet::new();
    let mut hosts = HashSet::new();

    for entry in &entries {
        if entry.name.trim().is_empty() {
            return Err("resolver name must not be empty".to_owned());
        }
        if !names.insert(entry.name.as_str()) {
            return Err("duplicate resolver name".to_owned());
        }
        let canonical_host = normalize_host_id(&entry.host)?;
        if !hosts.insert(canonical_host) {
            return Err("duplicate resolver host".to_owned());
        }
        if entry.regions.is_empty() {
            return Err("resolver has no regions".to_owned());
        }
        let mut regions = HashSet::new();
        if entry.regions.iter().any(|region| !regions.insert(*region)) {
            return Err("duplicate region for resolver".to_owned());
        }
    }

    Ok(entries)
}

pub(crate) fn normalize_host_id(host: &str) -> Result<String, String> {
    if host.is_empty()
        || host != host.trim()
        || host.chars().any(|character| {
            character.is_whitespace()
                || character.is_control()
                || matches!(character, ':' | '@' | '/' | '\\' | '?' | '#' | '[' | ']')
        })
    {
        return Err("resolver host must be a bare hostname".to_owned());
    }
    let url =
        Url::parse(&format!("https://{host}/")).map_err(|_| "invalid resolver host".to_owned())?;
    if url.scheme() != "https"
        || !matches!(url.host(), Some(Host::Domain(_)))
        || !url.username().is_empty()
        || url.password().is_some()
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err("resolver host is not a canonical HTTPS origin host".to_owned());
    }
    Ok(url.host_str().expect("domain host was checked").to_owned())
}

pub(crate) fn embedded_catalog() -> Result<Vec<ResolverEntry>, String> {
    load_catalog(RESOLVER_CATALOG_JSON)
}

#[cfg(test)]
mod tests {
    use super::{RESOLVER_CATALOG_JSON, embedded_catalog, load_catalog};

    #[test]
    fn embedded_catalog_has_43_valid_entries() {
        assert_eq!(
            embedded_catalog().expect("catalog should validate").len(),
            43
        );
    }

    #[test]
    fn parses_supported_regions_and_rejects_unknown_region() {
        for region in ["all", "cn", "hk", "tw", "th"] {
            let json = format!(r#"[{{"name":"one","host":"one.example","regions":["{region}"]}}]"#);
            assert!(load_catalog(&json).is_ok(), "rejected region {region}");
        }
        assert!(load_catalog(r#"[{"name":"one","host":"one.example","regions":["us"]}]"#).is_err());
    }

    #[test]
    fn rejects_duplicate_names_and_hosts() {
        let duplicate_name = r#"[
            {"name":"one","host":"one.example","regions":["all"]},
            {"name":"one","host":"two.example","regions":["cn"]}
        ]"#;
        let duplicate_host = r#"[
            {"name":"one","host":"same.example","regions":["all"]},
            {"name":"two","host":"same.example","regions":["cn"]}
        ]"#;
        assert!(
            load_catalog(duplicate_name)
                .unwrap_err()
                .contains("duplicate resolver name")
        );
        assert!(
            load_catalog(duplicate_host)
                .unwrap_err()
                .contains("duplicate resolver host")
        );
        let duplicate_host_case = r#"[
            {"name":"one","host":"Example.com","regions":["all"]},
            {"name":"two","host":"example.com","regions":["cn"]}
        ]"#;
        assert!(
            load_catalog(duplicate_host_case)
                .unwrap_err()
                .contains("duplicate resolver host")
        );
    }

    #[test]
    fn rejects_invalid_hosts_duplicate_regions_and_secret_fields() {
        for host in [
            "",
            "user:pass@example.com",
            "example.com:443",
            "example.com/path",
            "example.com?q=x",
        ] {
            let json = format!(r#"[{{"name":"bad","host":"{host}","regions":["all"]}}]"#);
            assert!(load_catalog(&json).is_err(), "accepted host {host:?}");
        }
        let duplicate_regions = r#"[{"name":"one","host":"one.example","regions":["all","all"]}]"#;
        assert!(
            load_catalog(duplicate_regions)
                .unwrap_err()
                .contains("duplicate region")
        );
        let secret_url_field = r#"[{"name":"one","host":"one.example","regions":["all"],"url":"https://user:secret@example.com/?token=x"}]"#;
        assert!(
            load_catalog(secret_url_field)
                .unwrap_err()
                .contains("invalid resolver catalog JSON")
        );
        let error =
            load_catalog(r#"[{"name":"bad","host":"user:secret@example.com","regions":["all"]}]"#)
                .unwrap_err();
        assert!(!error.contains("secret"));
    }

    #[test]
    fn embedded_json_is_the_expected_source_payload() {
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(RESOLVER_CATALOG_JSON)
                .unwrap()
                .as_array()
                .unwrap()
                .len(),
            43
        );
    }
}
