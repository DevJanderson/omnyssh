//! `ProxyJump` chain resolution.
//!
//! Turns the `ProxyJump` value of a [`Host`] (`bastion`, `ops@jump:2222`,
//! `first,second`, …) into the ordered list of hosts that must be connected
//! **before** the target, nearest to the local machine first.
//!
//! Each hop is looked up in the known host list — the merged `hosts.toml` +
//! `~/.ssh/config` entries — so a jump alias inherits that entry's `HostName`,
//! `User`, `Port` and `IdentityFile`, exactly like an `ssh -J` hop resolves
//! through `ssh_config`. A hop that matches no known entry is used literally as
//! a hostname. A hop with a `ProxyJump` of its own is expanded first, so nested
//! bastions work.
//!
//! Pure and I/O-free: [`resolve_chain`] takes the known hosts as an argument so
//! it can be unit-tested without touching the filesystem.

use std::collections::HashSet;

use anyhow::bail;

use crate::ssh::client::Host;

/// Upper bound on hops in a resolved chain. Longer chains are almost certainly
/// a configuration mistake; the limit also bounds the expansion recursion.
const MAX_HOPS: usize = 10;

/// One hop parsed from a `ProxyJump` value: `[user@]host[:port]`.
#[derive(Debug, PartialEq)]
struct JumpSpec {
    user: Option<String>,
    host: String,
    port: Option<u16>,
}

/// Resolves the full jump chain for `target` against the `known` host list.
///
/// The returned hosts are in connection order: the first entry is reached
/// directly from this machine, each subsequent one through its predecessor, and
/// `target` itself through the last. An empty vector means "connect directly" —
/// no `ProxyJump`, or the OpenSSH `ProxyJump none` opt-out.
///
/// Every returned host has its own `proxy_jump` cleared: the chain is already
/// flattened, so a caller connecting hop by hop must not expand it again.
///
/// # Errors
/// Returns an error when the chain references itself (a cycle) or exceeds
/// [`MAX_HOPS`] hops — both would otherwise loop forever at connect time.
pub fn resolve_chain(target: &Host, known: &[Host]) -> anyhow::Result<Vec<Host>> {
    let Some(spec) = jump_value(target) else {
        return Ok(Vec::new());
    };

    let mut chain: Vec<Host> = Vec::new();
    let mut visited: HashSet<String> = HashSet::new();
    // Seed with the target so `A -> B -> A` is caught as the cycle it is.
    mark_visited(target, &mut visited);
    expand(spec, known, &mut chain, &mut visited)?;
    Ok(chain)
}

/// The effective `ProxyJump` value of `host`, or `None` when it connects
/// directly. Blank values and the OpenSSH `none` opt-out both mean "direct".
fn jump_value(host: &Host) -> Option<&str> {
    let value = host.proxy_jump.as_deref()?.trim();
    if value.is_empty() || value.eq_ignore_ascii_case("none") {
        None
    } else {
        Some(value)
    }
}

/// Appends the hops of `spec` to `chain`, depth-first: a hop that jumps through
/// another host contributes that host first.
fn expand(
    spec: &str,
    known: &[Host],
    chain: &mut Vec<Host>,
    visited: &mut HashSet<String>,
) -> anyhow::Result<()> {
    for hop in parse_jump_spec(spec) {
        let mut host = resolve_hop(&hop, known);

        if !mark_visited(&host, visited) {
            bail!("ProxyJump cycle detected at '{}'", host.name);
        }

        // A bastion reached through another bastion: connect the inner one first.
        if let Some(nested) = jump_value(&host) {
            expand(nested, known, chain, visited)?;
        }
        if chain.len() >= MAX_HOPS {
            bail!("ProxyJump chain longer than {MAX_HOPS} hops");
        }
        host.proxy_jump = None;
        chain.push(host);
    }
    Ok(())
}

/// Turns one parsed hop into a connectable [`Host`].
///
/// A hop naming a known entry inherits all of its connection settings; anything
/// else becomes a bare host with default user and port. An explicit `user@` or
/// `:port` in the spec always wins over the inherited value.
fn resolve_hop(spec: &JumpSpec, known: &[Host]) -> Host {
    let mut host = match known.iter().find(|h| h.name == spec.host) {
        Some(h) => h.clone(),
        None => Host {
            name: spec.host.clone(),
            hostname: spec.host.clone(),
            ..Host::default()
        },
    };
    if let Some(user) = &spec.user {
        host.user = user.clone();
    }
    if let Some(port) = spec.port {
        host.port = port;
    }
    // A known entry may omit HostName; the alias is then the address (the same
    // fallback the ssh_config parser applies).
    if host.hostname.is_empty() {
        host.hostname = host.name.clone();
    }
    host
}

/// Records `host` as part of the chain being built, returning `false` when it
/// was already there — a cycle.
///
/// A hop counts as seen both by alias and by endpoint, so a loop is caught
/// whether it comes back under the same name or under a second alias for the
/// same machine.
fn mark_visited(host: &Host, visited: &mut HashSet<String>) -> bool {
    let by_name = visited.insert(format!("name:{}", host.name));
    let by_address = visited.insert(format!(
        "address:{}@{}:{}",
        host.user, host.hostname, host.port
    ));
    by_name && by_address
}

/// Splits a `ProxyJump` value into its comma-separated hops, nearest first.
///
/// Unparseable hops (an empty entry, a non-numeric port) are skipped rather
/// than failing the whole connection — the remaining hops still describe a
/// usable route, and an unreachable one surfaces as a normal connection error.
fn parse_jump_spec(value: &str) -> Vec<JumpSpec> {
    value.split(',').filter_map(parse_hop).collect()
}

/// Parses a single `[user@]host[:port]` hop. Bracketed IPv6 literals
/// (`[2001:db8::1]:2222`) are supported, matching `ssh -J`.
fn parse_hop(hop: &str) -> Option<JumpSpec> {
    let hop = hop.trim();
    if hop.is_empty() {
        return None;
    }

    // Split on the last '@': a username cannot contain one, a host never does.
    let (user, rest) = match hop.rsplit_once('@') {
        Some((user, rest)) if !user.is_empty() => (Some(user.to_string()), rest),
        _ => (None, hop),
    };

    let (host, port) = split_host_port(rest)?;
    if host.is_empty() {
        return None;
    }
    Some(JumpSpec {
        user,
        host: host.to_string(),
        port,
    })
}

/// Splits `host`, `host:port`, `[v6]` or `[v6]:port` into its two parts.
/// Returns `None` when a port is present but not a valid number.
fn split_host_port(rest: &str) -> Option<(&str, Option<u16>)> {
    // `end` indexes the ']' relative to the stripped string, i.e. the last
    // character of the address inside the brackets.
    if let Some(end) = rest.strip_prefix('[').and_then(|r| r.find(']')) {
        let host = &rest[1..=end];
        return match rest[end + 2..].strip_prefix(':') {
            Some(port) => Some((host, Some(port.parse().ok()?))),
            None => Some((host, None)),
        };
    }
    // An unbracketed colon separates the port only when it is the sole one;
    // a bare IPv6 literal has several and carries no port.
    match rest.split_once(':') {
        Some((host, port)) if !port.contains(':') => Some((host, Some(port.parse().ok()?))),
        Some(_) => Some((rest, None)),
        None => Some((rest, None)),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn host(name: &str, hostname: &str) -> Host {
        Host {
            name: name.to_string(),
            hostname: hostname.to_string(),
            user: "ops".to_string(),
            ..Host::default()
        }
    }

    fn jumping(name: &str, hostname: &str, via: &str) -> Host {
        Host {
            proxy_jump: Some(via.to_string()),
            ..host(name, hostname)
        }
    }

    // --- spec parsing ------------------------------------------------------

    #[test]
    fn parses_a_bare_alias() {
        assert_eq!(
            parse_jump_spec("bastion"),
            vec![JumpSpec {
                user: None,
                host: "bastion".into(),
                port: None
            }]
        );
    }

    #[test]
    fn parses_user_host_and_port() {
        assert_eq!(
            parse_jump_spec("ops@jump.example.com:2222"),
            vec![JumpSpec {
                user: Some("ops".into()),
                host: "jump.example.com".into(),
                port: Some(2222)
            }]
        );
    }

    #[test]
    fn parses_a_multi_hop_value_in_order() {
        let hops = parse_jump_spec("first, ops@second:2222");
        assert_eq!(hops.len(), 2);
        assert_eq!(hops[0].host, "first");
        assert_eq!(hops[1].host, "second");
        assert_eq!(hops[1].port, Some(2222));
    }

    #[test]
    fn parses_ipv6_literals() {
        let bare = parse_jump_spec("2001:db8::1");
        assert_eq!(bare[0].host, "2001:db8::1");
        assert_eq!(bare[0].port, None);

        let bracketed = parse_jump_spec("ops@[2001:db8::1]:2222");
        assert_eq!(bracketed[0].host, "2001:db8::1");
        assert_eq!(bracketed[0].port, Some(2222));
        assert_eq!(bracketed[0].user.as_deref(), Some("ops"));
    }

    #[test]
    fn skips_unusable_hops() {
        assert!(parse_jump_spec("").is_empty());
        assert!(parse_jump_spec(" , ").is_empty());
        assert!(parse_jump_spec("host:not-a-port").is_empty());
    }

    // --- chain resolution --------------------------------------------------

    #[test]
    fn no_proxy_jump_means_no_chain() {
        let target = host("web", "10.0.0.1");
        assert!(resolve_chain(&target, &[]).unwrap().is_empty());
    }

    #[test]
    fn proxy_jump_none_opts_out() {
        let target = jumping("web", "10.0.0.1", "none");
        assert!(resolve_chain(&target, &[]).unwrap().is_empty());
    }

    #[test]
    fn resolves_an_alias_against_the_known_hosts() {
        // The reported bug: `ProxyJump public-proxy` where `public-proxy` is
        // another entry in the same config.
        let known = vec![Host {
            port: 2222,
            identity_file: Some("/keys/proxy".into()),
            ..host("public-proxy", "proxy.example.com")
        }];
        let target = jumping("internal", "192.168.100.50", "public-proxy");

        let chain = resolve_chain(&target, &known).unwrap();
        assert_eq!(chain.len(), 1);
        assert_eq!(chain[0].hostname, "proxy.example.com");
        assert_eq!(chain[0].user, "ops");
        assert_eq!(chain[0].port, 2222);
        assert_eq!(chain[0].identity_file.as_deref(), Some("/keys/proxy"));
    }

    #[test]
    fn resolves_a_chain_parsed_straight_from_an_ssh_config() {
        // End to end over the two halves that must agree: what the parser
        // produces is what the resolver looks jump aliases up in.
        let cfg = "\
Host public-proxy
    HostName proxy.example.com
    User ops

Host internal
    HostName 192.168.100.50
    User admin
    ProxyJump public-proxy
";
        let hosts = crate::config::ssh_config::parse_ssh_config(cfg);
        let target = hosts.iter().find(|h| h.name == "internal").unwrap();

        let chain = resolve_chain(target, &hosts).unwrap();
        assert_eq!(chain.len(), 1);
        assert_eq!(chain[0].name, "public-proxy");
        assert_eq!(chain[0].hostname, "proxy.example.com");
        assert_eq!(chain[0].user, "ops");
        assert_eq!(chain[0].port, 22);
    }

    #[test]
    fn unknown_alias_falls_back_to_a_literal_host() {
        let target = jumping("internal", "10.0.0.2", "jump.example.com");
        let chain = resolve_chain(&target, &[]).unwrap();
        assert_eq!(chain.len(), 1);
        assert_eq!(chain[0].hostname, "jump.example.com");
        assert_eq!(chain[0].port, 22);
    }

    #[test]
    fn explicit_user_and_port_override_the_known_entry() {
        let known = vec![Host {
            port: 2222,
            ..host("public-proxy", "proxy.example.com")
        }];
        let target = jumping("internal", "10.0.0.2", "admin@public-proxy:2022");

        let chain = resolve_chain(&target, &known).unwrap();
        assert_eq!(chain[0].hostname, "proxy.example.com"); // still inherited
        assert_eq!(chain[0].user, "admin");
        assert_eq!(chain[0].port, 2022);
    }

    #[test]
    fn a_hostname_less_entry_uses_its_alias_as_the_address() {
        let known = vec![Host {
            hostname: String::new(),
            ..host("public-proxy", "")
        }];
        let target = jumping("internal", "10.0.0.2", "public-proxy");
        assert_eq!(
            resolve_chain(&target, &known).unwrap()[0].hostname,
            "public-proxy"
        );
    }

    #[test]
    fn multi_hop_chains_keep_connection_order() {
        let known = vec![host("first", "10.0.0.1"), host("second", "10.0.0.2")];
        let target = jumping("internal", "10.0.0.3", "first,second");

        let chain = resolve_chain(&target, &known).unwrap();
        let names: Vec<&str> = chain.iter().map(|h| h.name.as_str()).collect();
        assert_eq!(names, ["first", "second"]);
    }

    #[test]
    fn a_nested_jump_host_is_connected_first() {
        let known = vec![
            host("outer", "10.0.0.1"),
            jumping("inner", "10.0.0.2", "outer"),
        ];
        let target = jumping("internal", "10.0.0.3", "inner");

        let chain = resolve_chain(&target, &known).unwrap();
        let names: Vec<&str> = chain.iter().map(|h| h.name.as_str()).collect();
        assert_eq!(names, ["outer", "inner"]);
        // Flattened: the caller connects hop by hop and must not re-expand.
        assert!(chain.iter().all(|h| h.proxy_jump.is_none()));
    }

    #[test]
    fn a_self_referencing_jump_is_rejected() {
        let known = vec![jumping("loop", "10.0.0.1", "loop")];
        let target = jumping("internal", "10.0.0.2", "loop");
        let err = resolve_chain(&target, &known).unwrap_err().to_string();
        assert!(err.contains("cycle"), "unexpected error: {err}");
    }

    #[test]
    fn two_aliases_for_one_bastion_are_rejected() {
        // Same machine, different name: still a loop, just a less obvious one.
        let known = vec![
            jumping("proxy-a", "10.0.0.1", "proxy-b"),
            host("proxy-b", "10.0.0.1"),
        ];
        let target = jumping("internal", "10.0.0.2", "proxy-a");
        assert!(resolve_chain(&target, &known).is_err());
    }

    #[test]
    fn a_jump_back_to_the_target_is_rejected() {
        let known = vec![jumping("bastion", "10.0.0.1", "internal")];
        let target = jumping("internal", "10.0.0.2", "bastion");
        assert!(resolve_chain(&target, &known).is_err());
    }

    #[test]
    fn an_over_long_chain_is_rejected() {
        let hops: Vec<String> = (0..MAX_HOPS + 1).map(|i| format!("h{i}")).collect();
        let known: Vec<Host> = hops
            .iter()
            .enumerate()
            .map(|(i, name)| host(name, &format!("10.0.0.{i}")))
            .collect();
        let target = jumping("internal", "10.1.0.1", &hops.join(","));
        assert!(resolve_chain(&target, &known).is_err());
    }
}
