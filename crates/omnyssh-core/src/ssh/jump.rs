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
//! a hostname.
//!
//! Pure and I/O-free: [`resolve_chain`] takes the known hosts as an argument so
//! it can be unit-tested without touching the filesystem.

use anyhow::{anyhow, bail, Context};

use crate::ssh::client::Host;

/// Upper bound on the hops in a resolved chain, and on the depth of the
/// expansion recursion. Longer chains are almost certainly a configuration
/// mistake.
const MAX_HOPS: usize = 10;

/// One hop parsed from a `ProxyJump` value: `[user@]host[:port]`.
#[derive(Debug, PartialEq)]
struct JumpSpec {
    user: Option<String>,
    host: String,
    port: Option<u16>,
}

/// One chain walk in progress.
struct Walk {
    /// Hops resolved so far, in connection order.
    chain: Vec<Host>,
    /// Hops whose own `ProxyJump` is being expanded right now, plus the target
    /// that started the walk. Re-entering one of these is a cycle; the length
    /// is the recursion depth.
    active: Vec<Host>,
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
/// Returns an error when a hop is unusable, when the chain references itself (a
/// cycle), or when it exceeds [`MAX_HOPS`] hops. A host that names a bastion
/// never resolves to an empty chain: failing is the only alternative to
/// connecting straight to the target, past the bastion that is its only route.
pub fn resolve_chain(target: &Host, known: &[Host]) -> anyhow::Result<Vec<Host>> {
    let Some(spec) = jump_value(target) else {
        return Ok(Vec::new());
    };

    let mut walk = Walk {
        chain: Vec::new(),
        // Seed with the target so `A -> B -> A` is caught as the cycle it is.
        active: vec![target.clone()],
    };
    walk.expand(spec, known)?;
    Ok(walk.chain)
}

/// The effective `ProxyJump` value of `host`, or `None` when it connects
/// directly. Blank values and the OpenSSH `none` opt-out both mean "direct".
pub(crate) fn jump_value(host: &Host) -> Option<&str> {
    let value = host.proxy_jump.as_deref()?.trim();
    if value.is_empty() || value.eq_ignore_ascii_case("none") {
        None
    } else {
        Some(value)
    }
}

impl Walk {
    /// Appends the hops of `spec` to the chain, nearest hop first.
    ///
    /// Only the *first* hop of a list carries bastions of its own. That is what
    /// `ssh` does: for `ProxyJump a,b` it reaches `b` with `-J a` on the command
    /// line, and a command-line jump list makes it ignore `b`'s own configured
    /// `ProxyJump`. `a` is then reached by a plain `ssh`, which does read its
    /// `ProxyJump` — so nested bastions still work, one level in from each list.
    fn expand(&mut self, spec: &str, known: &[Host]) -> anyhow::Result<()> {
        let hops = parse_jump_spec(spec)?;

        for (index, hop) in hops.iter().enumerate() {
            let mut host = resolve_hop(hop, known);

            if self.active.iter().any(|h| same_hop(h, &host)) {
                bail!("ProxyJump cycle detected at '{}'", host.name);
            }
            let nested = if index == 0 { jump_value(&host) } else { None };
            if let Some(nested) = nested {
                if self.active.len() > MAX_HOPS {
                    bail!("ProxyJump chain nested deeper than {MAX_HOPS} hops");
                }
                self.active.push(host.clone());
                let expanded = self.expand(nested, known);
                self.active.pop();
                expanded?;
            }

            if self.chain.len() >= MAX_HOPS {
                bail!("ProxyJump chain longer than {MAX_HOPS} hops");
            }
            host.proxy_jump = None;
            self.chain.push(host);
        }
        Ok(())
    }
}

/// Whether two hops are the same machine — the same alias, or the same
/// endpoint reached under a second name. Only ever asked of hops still being
/// expanded, so a match is a back-edge, not a repetition.
fn same_hop(a: &Host, b: &Host) -> bool {
    a.name == b.name || (a.user == b.user && a.hostname == b.hostname && a.port == b.port)
}

/// Turns one parsed hop into a connectable [`Host`].
///
/// A hop naming a known entry inherits all of its connection settings; anything
/// else becomes a bare host with default user and port. An explicit `user@` or
/// `:port` in the spec always wins over the inherited value.
fn resolve_hop(spec: &JumpSpec, known: &[Host]) -> Host {
    // A host imported from `~/.ssh/config` and then renamed keeps its original
    // alias, which is still what every other entry's `ProxyJump` names — but an
    // entry that carries the alias as its own name comes first.
    let entry = known.iter().find(|h| h.name == spec.host).or_else(|| {
        known
            .iter()
            .find(|h| h.original_ssh_host.as_deref() == Some(spec.host.as_str()))
    });

    let mut host = match entry {
        Some(h) => h.clone(),
        None => {
            // Nothing to inherit. Worth a line: an alias that was meant to match
            // an entry now resolves through DNS instead.
            tracing::debug!(
                hop = %spec.host,
                "ProxyJump hop matches no known host; using it as a hostname"
            );
            Host {
                name: spec.host.clone(),
                hostname: spec.host.clone(),
                ..Host::default()
            }
        }
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

/// Splits a `ProxyJump` value into its comma-separated hops, nearest first.
///
/// # Errors
/// An unusable hop fails the whole value. Dropping it would shorten the route,
/// and dropping the only hop would connect straight to the target — past the
/// bastion the value exists to name.
fn parse_jump_spec(value: &str) -> anyhow::Result<Vec<JumpSpec>> {
    value.split(',').map(parse_hop).collect()
}

/// Parses a single `[user@]host[:port]` hop. Bracketed IPv6 literals
/// (`[2001:db8::1]:2222`) are supported, matching `ssh -J`.
fn parse_hop(hop: &str) -> anyhow::Result<JumpSpec> {
    let hop = hop.trim();
    if hop.is_empty() {
        bail!("empty ProxyJump hop");
    }

    // Split on the last '@': a username cannot contain one, a host never does.
    let (user, rest) = match hop.rsplit_once('@') {
        Some((user, rest)) if !user.is_empty() => (Some(user.to_string()), rest),
        _ => (None, hop),
    };

    let (host, port) =
        split_host_port(rest).map_err(|e| anyhow!("unusable ProxyJump hop '{hop}': {e:#}"))?;
    if host.is_empty() {
        bail!("ProxyJump hop '{hop}' has no host");
    }
    Ok(JumpSpec {
        user,
        host: host.to_string(),
        port,
    })
}

/// Splits `host`, `host:port`, `[v6]` or `[v6]:port` into its two parts.
fn split_host_port(rest: &str) -> anyhow::Result<(&str, Option<u16>)> {
    // `end` indexes the ']' relative to the stripped string, i.e. the last
    // character of the address inside the brackets.
    if let Some(end) = rest.strip_prefix('[').and_then(|r| r.find(']')) {
        let host = &rest[1..=end];
        let trailer = &rest[end + 2..];
        if trailer.is_empty() {
            return Ok((host, None));
        }
        let port = trailer
            .strip_prefix(':')
            .with_context(|| format!("trailing '{trailer}' after ']'"))?;
        return Ok((host, Some(parse_port(port)?)));
    }
    // An unbracketed colon separates the port only when it is the sole one;
    // a bare IPv6 literal has several and carries no port.
    match rest.split_once(':') {
        Some((host, port)) if !port.contains(':') => Ok((host, Some(parse_port(port)?))),
        _ => Ok((rest, None)),
    }
}

/// Parses a hop's port. Zero is rejected the way `ssh` rejects it — it can
/// never name a listening service.
fn parse_port(value: &str) -> anyhow::Result<u16> {
    match value.parse::<u16>() {
        Ok(0) | Err(_) => bail!("'{value}' is not a valid port"),
        Ok(port) => Ok(port),
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

    fn names(chain: &[Host]) -> Vec<&str> {
        chain.iter().map(|h| h.name.as_str()).collect()
    }

    // --- spec parsing ------------------------------------------------------

    #[test]
    fn parses_a_bare_alias() {
        assert_eq!(
            parse_jump_spec("bastion").unwrap(),
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
            parse_jump_spec("ops@jump.example.com:2222").unwrap(),
            vec![JumpSpec {
                user: Some("ops".into()),
                host: "jump.example.com".into(),
                port: Some(2222)
            }]
        );
    }

    #[test]
    fn parses_a_multi_hop_value_in_order() {
        let hops = parse_jump_spec("first, ops@second:2222").unwrap();
        assert_eq!(hops.len(), 2);
        assert_eq!(hops[0].host, "first");
        assert_eq!(hops[1].host, "second");
        assert_eq!(hops[1].port, Some(2222));
    }

    #[test]
    fn parses_ipv6_literals() {
        let bare = parse_jump_spec("2001:db8::1").unwrap();
        assert_eq!(bare[0].host, "2001:db8::1");
        assert_eq!(bare[0].port, None);

        let bracketed = parse_jump_spec("ops@[2001:db8::1]:2222").unwrap();
        assert_eq!(bracketed[0].host, "2001:db8::1");
        assert_eq!(bracketed[0].port, Some(2222));
        assert_eq!(bracketed[0].user.as_deref(), Some("ops"));

        assert_eq!(parse_jump_spec("[2001:db8::1]").unwrap()[0].port, None);
    }

    #[test]
    fn rejects_unusable_hops() {
        // Dropping any of these would silently shorten the route.
        for value in [
            "",
            " , ",
            "host:not-a-port",
            "host:",
            "host:70000",
            "host:0",
            "gw: 2222",
            "ops@",
            "[2001:db8::1]junk:22",
            "first,bad:port",
        ] {
            assert!(
                parse_jump_spec(value).is_err(),
                "expected '{value}' to be rejected"
            );
        }
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
    fn a_malformed_value_fails_instead_of_connecting_direct() {
        // The whole point of the module: never silently return an empty chain
        // for a host that names a bastion.
        let target = jumping("internal", "192.168.100.50", "public-proxy:22x");
        assert!(resolve_chain(&target, &[]).is_err());
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
    fn a_renamed_bastion_is_still_found_under_its_original_alias() {
        // Editing an imported host renames it and records the original; every
        // other entry's ProxyJump still names the original.
        let known = vec![Host {
            original_ssh_host: Some("public-proxy".into()),
            ..host("Prod Bastion", "proxy.example.com")
        }];
        let target = jumping("internal", "10.0.0.2", "public-proxy");
        assert_eq!(
            resolve_chain(&target, &known).unwrap()[0].hostname,
            "proxy.example.com"
        );

        // An entry that owns the alias outright wins over one that used to.
        let known = vec![
            Host {
                original_ssh_host: Some("public-proxy".into()),
                ..host("Prod Bastion", "renamed.example.com")
            },
            host("public-proxy", "proxy.example.com"),
        ];
        assert_eq!(
            resolve_chain(&target, &known).unwrap()[0].hostname,
            "proxy.example.com"
        );
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
        assert_eq!(names(&chain), ["first", "second"]);
    }

    #[test]
    fn a_nested_jump_host_is_connected_first() {
        let known = vec![
            host("outer", "10.0.0.1"),
            jumping("inner", "10.0.0.2", "outer"),
        ];
        let target = jumping("internal", "10.0.0.3", "inner");

        let chain = resolve_chain(&target, &known).unwrap();
        assert_eq!(names(&chain), ["outer", "inner"]);
        // Flattened: the caller connects hop by hop and must not re-expand.
        assert!(chain.iter().all(|h| h.proxy_jump.is_none()));
    }

    #[test]
    fn only_the_first_hop_of_a_list_expands_its_own_bastion() {
        // `ssh -J first,second` reaches `second` with the jump list on the
        // command line, which outranks `second`'s configured ProxyJump.
        let known = vec![
            host("edge", "10.0.0.1"),
            host("first", "10.0.0.2"),
            jumping("second", "10.0.0.3", "edge"),
        ];
        let target = jumping("internal", "10.0.0.4", "first,second");
        assert_eq!(
            names(&resolve_chain(&target, &known).unwrap()),
            ["first", "second"]
        );

        // …but the first hop's own bastion still applies.
        let target = jumping("internal", "10.0.0.4", "second,first");
        assert_eq!(
            names(&resolve_chain(&target, &known).unwrap()),
            ["edge", "second", "first"]
        );
    }

    #[test]
    fn a_bastion_named_twice_is_not_a_cycle() {
        // `edge` is both a hop of the list and `inner`'s own bastion. It is
        // already connected by the time it comes round again — a repetition,
        // not a loop to refuse. `ssh` reaches the last hop of a list last, so
        // the second mention keeps its place rather than being dropped.
        let known = vec![
            host("edge", "10.0.0.1"),
            jumping("inner", "10.0.0.2", "edge"),
        ];

        let target = jumping("internal", "10.0.0.3", "inner,edge");
        assert_eq!(
            names(&resolve_chain(&target, &known).unwrap()),
            ["edge", "inner", "edge"]
        );

        let target = jumping("internal", "10.0.0.3", "edge,inner");
        assert_eq!(
            names(&resolve_chain(&target, &known).unwrap()),
            ["edge", "inner"]
        );
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
        // Same machine, different name: reaching it would require reaching it.
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
    fn the_hop_limit_is_the_boundary_it_claims() {
        let chain_of = |count: usize| {
            let hops: Vec<String> = (0..count).map(|i| format!("h{i}")).collect();
            let known: Vec<Host> = hops
                .iter()
                .enumerate()
                .map(|(i, name)| host(name, &format!("10.0.0.{i}")))
                .collect();
            let target = jumping("internal", "10.1.0.1", &hops.join(","));
            resolve_chain(&target, &known)
        };

        assert_eq!(chain_of(MAX_HOPS).unwrap().len(), MAX_HOPS);
        assert!(chain_of(MAX_HOPS + 1).is_err());
    }

    #[test]
    fn a_deeply_nested_chain_is_rejected_before_it_recurses_away() {
        // Each entry jumps through the next, so the walk descends without ever
        // appending to the chain — the depth bound is what has to catch it.
        let known: Vec<Host> = (0..MAX_HOPS * 4)
            .map(|i| {
                jumping(
                    &format!("h{i}"),
                    &format!("10.0.0.{i}"),
                    &format!("h{}", i + 1),
                )
            })
            .collect();
        let target = jumping("internal", "10.1.0.1", "h0");

        // The depth bound has to be what stops it: the chain-length bound reads
        // a chain that is still empty this far down.
        let err = resolve_chain(&target, &known).unwrap_err().to_string();
        assert!(err.contains("nested deeper"), "unexpected error: {err}");
    }
}
