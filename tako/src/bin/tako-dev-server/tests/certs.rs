use super::*;

/// Verify that the dynamic cert resolver generates a cert whose SAN
/// exactly matches the requested hostname - this is how we sidestep
/// OpenSSL rejecting `*.tako` wildcards (single-label TLD).
#[test]
fn dev_cert_resolver_generates_cert_matching_hostname() {
    let ca = LocalCA::generate().unwrap();
    let resolver = DevCertResolver::new(ca);

    let (x509, _pkey) = resolver
        .get_or_create_cert("foo.test")
        .expect("should generate cert");

    // Verify the SAN contains the exact hostname.
    let pem = x509.to_pem().unwrap();
    let (_, parsed_pem) = x509_parser::pem::parse_x509_pem(&pem).unwrap();
    let parsed = parsed_pem.parse_x509().unwrap();

    let san_ext = parsed
        .extensions()
        .iter()
        .find(|ext| ext.oid == x509_parser::oid_registry::OID_X509_EXT_SUBJECT_ALT_NAME)
        .expect("cert must have SAN extension");

    let san = match san_ext.parsed_extension() {
        x509_parser::extensions::ParsedExtension::SubjectAlternativeName(san) => san,
        other => panic!("expected SubjectAlternativeName, got {:?}", other),
    };

    let dns_names: Vec<&str> = san
        .general_names
        .iter()
        .filter_map(|n| match n {
            x509_parser::extensions::GeneralName::DNSName(d) => Some(*d),
            _ => None,
        })
        .collect();

    assert!(
        dns_names.contains(&"foo.test"),
        "cert must contain foo.test SAN, got: {:?}",
        dns_names
    );
}

/// Verify that the dynamically generated cert chains back to the CA
/// and that the SAN exactly matches — these are the two checks that
/// Chrome/BoringSSL performs during the TLS handshake.

#[test]
fn dev_cert_resolver_cert_is_signed_by_ca() {
    let ca = LocalCA::generate().unwrap();
    let ca_x509 = X509::from_pem(ca.ca_cert_pem().as_bytes()).unwrap();
    let resolver = DevCertResolver::new(ca);

    let (leaf_x509, _) = resolver
        .get_or_create_cert("foo.test")
        .expect("should generate cert");

    // Verify the leaf cert is signed by the CA's public key.
    let ca_pubkey = ca_x509.public_key().unwrap();
    assert!(
        leaf_x509.verify(&ca_pubkey).unwrap(),
        "leaf cert must be signed by the local CA"
    );
}

#[test]
fn dev_cert_resolver_caches_certs() {
    let ca = LocalCA::generate().unwrap();
    let resolver = DevCertResolver::new(ca);

    let (first, _) = resolver.get_or_create_cert("bar.test").unwrap();
    let (second, _) = resolver.get_or_create_cert("bar.test").unwrap();

    // Same DER bytes → same cert object was returned from cache.
    assert_eq!(first.to_der().unwrap(), second.to_der().unwrap());
}
