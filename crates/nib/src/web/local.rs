//! The one rule for what the web tab may show: pages served by this machine, nothing else.
//!
//! It is a security boundary (D-072), so it is fixed code with no configuration: the tab must not
//! become a browser, and an agent that reads pages must not be led onto the web by a page. The
//! engine applies it to every address it is asked to open and to every document Firefox is
//! about to request (`engine.rs`); nothing else decides.
//!
//! Strict on purpose: an address is local only when its host is spelled in one of the plain
//! ways below. No name is looked up, and anything unusual is refused rather than understood.

use std::net::Ipv4Addr;

/// The parts of an address the rule looks at.
struct Parts<'a> {
    scheme: &'a str,
    host: &'a str,
    port: Option<&'a str>,
}

/// `scheme://host[:port]…` taken apart, or None for anything that is not exactly that shape.
fn parts(url: &str) -> Option<Parts<'_>> {
    // No control characters, spaces or backslashes anywhere: browsers "repair" those, so what
    // we would check and what would be fetched could differ.
    if url.chars().any(|c| c.is_control() || c == ' ' || c == '\\') {
        return None;
    }
    let (scheme, rest) = url.split_once("://")?;
    let authority = &rest[..rest.find(['/', '?', '#']).unwrap_or(rest.len())];
    // A user-info part is how `http://localhost@evil.example/` hides its host.
    if authority.is_empty() || authority.contains('@') {
        return None;
    }
    let (host, port) = if let Some(v6) = authority.strip_prefix('[') {
        let (host, after) = v6.split_once(']')?;
        match after {
            "" => (host, None),
            _ => (host, Some(after.strip_prefix(':')?)),
        }
    } else {
        match authority.rsplit_once(':') {
            Some((h, p)) => (h, Some(p)),
            None => (authority, None),
        }
    };
    if let Some(p) = port {
        if p.is_empty() || p.len() > 5 || !p.bytes().all(|b| b.is_ascii_digit()) || p.parse::<u16>().is_err() {
            return None;
        }
    }
    // Brackets are for IPv6 only, and only IPv6 has colons.
    let v6 = authority.starts_with('[');
    if host.is_empty() || (v6 != host.contains(':')) {
        return None;
    }
    Some(Parts { scheme, host, port })
}

/// A host that is this machine, spelled plainly.
fn local_host(host: &str, v6: bool) -> bool {
    if v6 {
        return host == "::1";
    }
    let h = host.to_ascii_lowercase();
    if h == "localhost" {
        return true;
    }
    if let Some(sub) = h.strip_suffix(".localhost") {
        // Labels of letters, digits and hyphens only.
        return !sub.is_empty() && sub.split('.').all(|l| !l.is_empty() && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'));
    }
    // 127.0.0.0/8 in plain dotted form: four decimal numbers, no leading zeros (which some
    // parsers read as octal), nothing shortened.
    let nums: Vec<&str> = h.split('.').collect();
    if nums.len() != 4 || nums.iter().any(|n| n.is_empty() || n.len() > 3 || (n.len() > 1 && n.starts_with('0')) || !n.bytes().all(|b| b.is_ascii_digit())) {
        return false;
    }
    h.parse::<Ipv4Addr>().is_ok_and(|ip| ip.octets()[0] == 127)
}

/// Whether `url` is a page of this machine: `http` or `https`, no user-info, and a host that is
/// exactly `localhost`, a name under `.localhost`, `127.x.x.x`, or `[::1]`.
pub fn is_local(url: &str) -> bool {
    let Some(p) = parts(url) else { return false };
    (p.scheme == "http" || p.scheme == "https") && local_host(p.host, url[p.scheme.len() + 3..].starts_with('['))
}

/// What a person or an agent asked to open, as a full address: `6006` and `localhost:6006/x`
/// get their `http://`, and `0.0.0.0` (which servers print for "every interface") becomes
/// `127.0.0.1`. The result still has to pass [`is_local`]; this only spells things out.
pub fn spell_out(asked: &str) -> String {
    let asked = asked.trim();
    let url = if !asked.is_empty() && asked.bytes().all(|b| b.is_ascii_digit()) {
        format!("http://127.0.0.1:{asked}/")
    } else if asked.contains("://") {
        asked.to_string()
    } else {
        format!("http://{asked}")
    };
    for scheme in ["http://", "https://"] {
        if let Some(rest) = url.strip_prefix(scheme) {
            let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
            if &rest[..end] == "0.0.0.0" || rest[..end].starts_with("0.0.0.0:") {
                return format!("{scheme}127.0.0.1{}", &rest["0.0.0.0".len()..]);
            }
        }
    }
    url
}

/// `host:port` of a local address, with every spelling of this machine as `localhost`: what
/// tells one app from another, and what a tab is called when its page has no title.
pub fn app_of(url: &str) -> Option<String> {
    let p = parts(url)?;
    if !is_local(url) {
        return None;
    }
    let port = p.port.map(str::to_owned).unwrap_or_else(|| if p.scheme == "https" { "443".into() } else { "80".into() });
    let host = match p.host.to_ascii_lowercase() {
        h if h.ends_with(".localhost") => h,
        _ => "localhost".to_string(),
    };
    Some(format!("{host}:{port}"))
}

/// The name of the machine a refused address points at, for telling the user what was not opened.
pub fn host_of(url: &str) -> String {
    parts(url).map(|p| p.host.to_string()).unwrap_or_else(|| url.chars().take(60).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn this_machine_spelled_plainly_is_local() {
        for u in [
            "http://localhost",
            "http://localhost/",
            "http://localhost:6006/#timeseries",
            "http://LOCALHOST:8000/docs?x=1",
            "https://localhost:8443/",
            "http://127.0.0.1:8765/api/health",
            "http://127.0.0.1",
            "http://127.1.2.3:80/",
            "http://[::1]:8888/lab?token=abc",
            "http://[::1]/",
            "http://app.localhost:3000/",
            "http://a.b-c.localhost/",
        ] {
            assert!(is_local(u), "{u} should be local");
        }
    }

    #[test]
    fn everything_else_is_refused() {
        for u in [
            // Other machines, however much they look like this one.
            "http://example.com/",
            "https://example.com",
            "http://localhost.evil.example/",
            "http://127.0.0.1.evil.example/",
            "http://evil.example/localhost",
            "http://evil.example/?u=http://localhost/",
            "http://evil.example#@localhost/",
            "http://localhostx/",
            "http://xlocalhost/",
            "http://.localhost/",
            "http://a..localhost/",
            "http://192.168.1.150:8000/",
            "http://10.0.0.1/",
            "http://0.0.0.0:8000/",
            "http://128.0.0.1/",
            // A user-info part hiding the host.
            "http://localhost@evil.example/",
            "http://localhost:80@evil.example/",
            "http://user:pw@localhost/",
            "http://127.0.0.1@evil.example/",
            // Other spellings of an address: refused, not understood.
            "http://2130706433/",
            "http://0x7f000001/",
            "http://0x7f.0.0.1/",
            "http://0177.0.0.1/",
            "http://127.1/",
            "http://127.0.0.01/",
            "http://127.0.0.1./",
            "http://localhost./",
            "http://[::ffff:127.0.0.1]/",
            "http://[0:0:0:0:0:0:0:1]/",
            "http://[::1/",
            "http://[::1]x/",
            "http://::1/",
            "http://[localhost]/",
            "http://１２７.0.0.1/",
            "http://ⓛocalhost/",
            // Ports that are not ports.
            "http://localhost:/",
            "http://localhost:99999/",
            "http://localhost:80a/",
            "http://localhost:-1/",
            // Other schemes and shapes.
            "file:///etc/passwd",
            "ftp://localhost/",
            "ws://localhost/",
            "javascript:alert(1)",
            "data:text/html,<p>hi",
            "about:blank",
            "view-source:http://localhost/",
            "localhost:8000",
            "//localhost/",
            "http:/localhost/",
            "http:localhost",
            "",
            "http://",
            "http:///path",
            // What a browser would quietly repair.
            "http://localhost\\@evil.example/",
            "http://local host/",
            "http://localhost\t/",
            "http://localhost\n.evil.example/",
            " http://localhost/",
        ] {
            assert!(!is_local(u), "{u:?} should be refused");
        }
    }

    #[test]
    fn what_was_asked_is_spelled_out_before_the_rule() {
        assert_eq!(spell_out("6006"), "http://127.0.0.1:6006/");
        assert_eq!(spell_out(" localhost:8000/docs "), "http://localhost:8000/docs");
        assert_eq!(spell_out("http://0.0.0.0:8000/x"), "http://127.0.0.1:8000/x");
        assert_eq!(spell_out("0.0.0.0:8000"), "http://127.0.0.1:8000");
        assert_eq!(spell_out("http://0.0.0.0"), "http://127.0.0.1");
        // Only the host is rewritten, and only when it is exactly that.
        assert_eq!(spell_out("http://0.0.0.0.evil.example/"), "http://0.0.0.0.evil.example/");
        assert_eq!(spell_out("http://evil.example/0.0.0.0"), "http://evil.example/0.0.0.0");
        assert!(!is_local(&spell_out("example.com")));
        assert!(!is_local(&spell_out("http://0.0.0.0.evil.example/")));
        assert!(is_local(&spell_out("0.0.0.0:8000")));
    }

    #[test]
    fn an_app_is_its_host_and_port() {
        assert_eq!(app_of("http://127.0.0.1:6006/#x").as_deref(), Some("localhost:6006"));
        assert_eq!(app_of("http://localhost:6006/").as_deref(), Some("localhost:6006"));
        assert_eq!(app_of("http://[::1]:6006/").as_deref(), Some("localhost:6006"));
        assert_eq!(app_of("http://localhost/").as_deref(), Some("localhost:80"));
        assert_eq!(app_of("https://localhost/").as_deref(), Some("localhost:443"));
        assert_eq!(app_of("http://api.localhost:3000/").as_deref(), Some("api.localhost:3000"));
        assert_eq!(app_of("http://example.com/"), None);
        assert_eq!(host_of("http://example.com:81/x"), "example.com");
    }
}
