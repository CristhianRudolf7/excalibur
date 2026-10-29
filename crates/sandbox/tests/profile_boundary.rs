use excalibur_sandbox::{
    SandboxConfig, SandboxProfile,
    network::{NetworkPolicy, ProxyConfig},
};
#[test]
fn effective_profile_cannot_remove_host_syscall_restrictions_or_enable_an_invalid_proxy_base() {
    let root = tempfile::tempdir().unwrap();
    let mut restricted = SandboxConfig::project(root.path());
    restricted.profile = SandboxProfile::ProjectRestricted;
    assert!(
        SandboxConfig::project(root.path())
            .restricted_by(&restricted)
            .is_err()
    );
    assert!(
        SandboxConfig::read_only(root.path())
            .restricted_by(&restricted)
            .is_ok()
    );
    let mut invalid_writable = SandboxConfig::read_only(root.path());
    invalid_writable.write_roots.push(root.path().to_owned());
    assert!(
        SandboxConfig::project(root.path())
            .restricted_by(&invalid_writable)
            .is_err()
    );
    let mut invalid = SandboxConfig::read_only(root.path());
    invalid.network = Some(ProxyConfig::new(NetworkPolicy::Full));
    let mut requested = invalid.clone();
    requested.profile = SandboxProfile::ProjectNet;
    assert!(requested.restricted_by(&invalid).is_err());
    let full = SandboxConfig::project_net(
        root.path(),
        ProxyConfig::new(NetworkPolicy::Full),
        "/trusted/helper",
    );
    assert!(
        SandboxConfig::project(root.path())
            .restricted_by(&full)
            .is_ok()
    );
    assert!(
        SandboxConfig::read_only(root.path())
            .restricted_by(&full)
            .is_ok()
    );
}
