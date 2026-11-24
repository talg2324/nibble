//! What the agents' commands (`nib-mail`, `nib-work`, `nib-review`, `nib-app`, `nib-web`, each
//! `nibd TOOL …`) do alike around their own work: the help, and how an answer is printed.

use std::path::PathBuf;

/// `-h`, `--help` or `help` first: print `usage` and exit 0. With `bare` (the tool does nothing
/// without words), no words at all too, exiting 2. `None`: go on.
pub fn help(args: &[String], usage: &str, bare: bool) -> Option<i32> {
    match args.first().map(String::as_str) {
        Some("-h" | "--help" | "help") => {
            println!("{usage}");
            Some(0)
        }
        None if bare => {
            println!("{usage}");
            Some(2)
        }
        _ => None,
    }
}

/// Print what a tool answered, on stdout when it succeeded and stderr when not, and give its
/// exit code back.
pub fn print((out, code): (String, i32)) -> i32 {
    if code == 0 {
        println!("{out}");
    } else {
        eprintln!("{out}");
    }
    code
}

/// This agent's own folder for requests of kind `sub` (`app`, `web`), with its `req/` and `res/`
/// made (see `rpc`), when nib started this agent (`mail::owner`). `None`: it did not, or the
/// folders cannot be made.
pub fn rpc_dir(sub: &str) -> Option<PathBuf> {
    let dir = crate::mail::owner().map(|(dir, me)| crate::mail::own(&dir, &me).join(sub))?;
    (std::fs::create_dir_all(dir.join("req")).is_ok() && std::fs::create_dir_all(dir.join("res")).is_ok()).then_some(dir)
}
