//! Who left a request in the mailbox. `nib-app` and `nib-web` are short-lived commands that
//! leave their requests as files in the session's mailbox folder; anything that can write
//! there could otherwise ask too, as anybody. So each agent session gets a random token in its
//! environment (`$NIB_TOKEN`, beside `$NIB_AGENT`), every request carries it, and the daemon
//! acts only on a request whose token it gave out, as the agent it gave it to.
//!
//! Tokens live in this process: they are as good as the daemon that runs the agents, and gone
//! with it.

use std::collections::hash_map::RandomState;
use std::collections::HashMap;
use std::hash::{BuildHasher, Hasher};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

/// The environment variable an agent's commands find their token in.
pub const VAR: &str = "NIB_TOKEN";

/// Token -> the agent it was given to (`claude-3`).
static GIVEN: Mutex<Option<HashMap<String, String>>> = Mutex::new(None);

/// 128 bits nobody can guess: two hashers with keys of their own, fresh from the system.
pub(crate) fn fresh() -> String {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    let half = |salt: u64| {
        let mut h = RandomState::new().build_hasher();
        h.write_u128(nanos);
        h.write_u64(salt);
        h.write_u32(std::process::id());
        h.finish()
    };
    format!("{:016x}{:016x}", half(1), half(2))
}

/// The token of `agent` (the same one for as long as the daemon runs).
pub fn issue(agent: &str) -> String {
    let mut given = GIVEN.lock().unwrap();
    let map = given.get_or_insert_with(HashMap::new);
    if let Some((t, _)) = map.iter().find(|(_, a)| *a == agent) {
        return t.clone();
    }
    let t = fresh();
    map.insert(t.clone(), agent.to_string());
    t
}

/// The agent `token` was given to, if it was.
pub fn who(token: &str) -> Option<String> {
    if token.is_empty() {
        return None;
    }
    GIVEN.lock().unwrap().as_ref()?.get(token).cloned()
}

/// Make `dir` (a mailbox folder) readable and writable by its owner only. On Windows the
/// folder is under the user's own profile, which other users cannot open already.
pub fn private(dir: &std::path::Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    }
    #[cfg(not(unix))]
    let _ = dir;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_token_names_its_agent_and_nothing_else_does() {
        let a = issue("claude-91");
        let b = issue("codex-92");
        assert_eq!(a.len(), 32);
        assert_ne!(a, b);
        assert_eq!(issue("claude-91"), a, "one token per agent");
        assert_eq!(who(&a).as_deref(), Some("claude-91"));
        assert_eq!(who(&b).as_deref(), Some("codex-92"));
        assert_eq!(who(""), None);
        assert_eq!(who("0123456789abcdef0123456789abcdef"), None);
    }
}
