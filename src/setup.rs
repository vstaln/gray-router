//! `gray-router setup`: write a template config and print the next steps.
//! Non-interactive like the smallest gray plugins: the file is the wizard —
//! the user edits tier endpoints in, exports the keys, `/route` works.

use crate::config;

/// Write `~/.gray/router.json` when missing (never overwrites), then print
/// the install instructions. `force` rewrites the template.
pub fn setup(force: bool) -> String {
    match run(force) {
        Ok(msg) => msg,
        Err(e) => format!("setup failed: {e:#}"),
    }
}

fn run(force: bool) -> anyhow::Result<String> {
    let path = config::config_path()?;
    let existed = path.exists();
    if !existed || force {
        let dir = path.parent().unwrap();
        std::fs::create_dir_all(dir)?;
        let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
        use std::io::Write;
        writeln!(tmp, "{}", serde_json::to_string_pretty(&config::template())?)?;
        tmp.persist(&path)?;
    }
    let mut out = String::new();
    if existed && !force {
        out.push_str(&format!("config already exists: {}\n", path.display()));
    } else {
        out.push_str(&format!("wrote {}\n", path.display()));
    }
    out.push_str(
        "\nNext:\n\
         1. Edit tiers in that file — each needs base_url, model, and a key\n\
         \x20  (`api_key_env` for real providers, `api_key` for local dev).\n\
         2. Install + connect: `gray plugin install <this dir>`, then in a\n\
         \x20  session `/connect` → Router (or `/route use`) and pick `auto`.\n\
         3. `/route fast|smart|cheap|auto|off` steers; `/route status` shows\n\
         \x20  the mode and last decision.\n",
    );
    Ok(out)
}
