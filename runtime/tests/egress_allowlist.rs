use std::fs;
use std::path::Path;

fn repo_file(path: &str) -> String {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("runtime has a repository root");
    fs::read_to_string(root.join(path)).expect("documented egress policy file exists")
}

fn uncommented(policy: &str) -> String {
    policy
        .lines()
        .map(|line| line.split_once('#').map_or(line, |(code, _)| code))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn cilium_policy_is_restrictive_and_lists_the_required_destinations() {
    let policy = repo_file("deploy/k8s/egress-allowlist-cilium.yaml");

    for required in [
        "kind: CiliumNetworkPolicy",
        "enableDefaultDeny:",
        "egress:",
        "toFQDNs:",
        "k8s:k8s-app",
        "registry.npmjs.org",
        "pypi.org",
        "files.pythonhosted.org",
        "index.crates.io",
        "static.crates.io",
        "deb.debian.org",
        "github.com",
        "gitlab.com",
        "api.openai.com",
        "api.anthropic.com",
        "controlplane.tailscale.com",
        "derp*-all.tailscale.com",
    ] {
        assert!(policy.contains(required), "Cilium policy lacks {required}");
    }

    let policy = uncommented(&policy);
    assert!(
        !policy.contains("0.0.0.0/0"),
        "an FQDN allowlist must not contain a broad IPv4 egress rule"
    );
    assert!(
        !policy.contains("::/0"),
        "an FQDN allowlist must not contain a broad IPv6 egress rule"
    );
    assert!(
        !policy.contains("matchPattern: \"*\"") && !policy.contains("matchPattern: *"),
        "the FQDN policy must not permit arbitrary DNS queries"
    );
}

#[test]
fn proxy_policy_is_limited_to_the_proxy_and_cluster_dns() {
    let policy = repo_file("deploy/k8s/egress-via-proxy.yaml");

    for required in [
        "kind: NetworkPolicy",
        "policyTypes: [\"Egress\"]",
        "app: egress-proxy",
        "k8s-app: kube-dns",
        "port: 3128",
        "port: 53",
    ] {
        assert!(policy.contains(required), "proxy policy lacks {required}");
    }

    let policy = uncommented(&policy);
    assert!(
        !policy.contains("0.0.0.0/0"),
        "proxy mode must not allow direct IPv4 internet egress"
    );
    assert!(
        !policy.contains("::/0"),
        "proxy mode must not allow direct IPv6 internet egress"
    );
}

#[test]
fn operator_docs_explain_forwarded_proxy_environment_and_limits() {
    let k8s = repo_file("docs/k8s-howto.md");
    let ecs = repo_file("docs/ecsctl-howto.md");

    for required in [
        "egress-allowlist-cilium.yaml",
        "egress-via-proxy.yaml",
        "networkpolicy-no-tailnet-egress.yaml",
        "NetworkPolicy rules are additive",
    ] {
        assert!(k8s.contains(required), "Kubernetes docs lack {required}");
    }

    for required in [
        "PTY_FORWARD_HTTPS_PROXY",
        "PTY_FORWARD_NO_PROXY",
        "advisory",
        "repo-scoped git credentials",
        "read-only registry tokens",
    ] {
        assert!(ecs.contains(required), "ECS docs lack {required}");
    }
}
