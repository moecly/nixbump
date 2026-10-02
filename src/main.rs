use std::io::{self, IsTerminal, Write};
use std::path::Path;
use std::process::{self, Command, Stdio};

use clap::Parser;
use regex::Regex;

#[derive(Parser)]
#[command(name = "nixbump", about = "Update version/url/hash of a nix fetchurl package")]
struct Cli {
    #[arg(long)]
    file: Option<String>,
    #[arg(long)]
    url: Option<String>,
    #[arg(long)]
    version: Option<String>,
    #[arg(long)]
    backup: bool,
}

fn main() {
    let cli = Cli::parse();
    let file = cli.file.unwrap_or_else(|| prompt("file path"));
    let url = cli.url.unwrap_or_else(|| prompt("new url"));
    let version = cli.version.unwrap_or_else(|| prompt("new version"));

    if let Err(e) = run(&file, &url, &version, cli.backup) {
        eprintln!("error: {e}");
        process::exit(2);
    }
}

fn fail(msg: &str) -> ! {
    eprintln!("error: {msg}");
    process::exit(2);
}

fn prompt(label: &str) -> String {
    let mut err = io::stderr();
    write!(err, "{label}: ").unwrap();
    err.flush().unwrap();
    let mut line = String::new();
    if io::stdin().read_line(&mut line).unwrap() == 0 {
        fail(&format!("empty input for {label}"));
    }
    let v = line.trim().to_string();
    if v.is_empty() {
        fail(&format!("empty input for {label}"));
    }
    v
}

/// 交互确认，默认 No（回车/EOF 视为拒绝）。
fn confirm(label: &str) -> bool {
    let mut err = io::stderr();
    write!(err, "{label} [y/N] ").unwrap();
    err.flush().unwrap();
    let mut line = String::new();
    if io::stdin().read_line(&mut line).unwrap_or(0) == 0 {
        return false;
    }
    matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

fn run(file: &str, url: &str, version: &str, backup: bool) -> Result<(), String> {
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err(format!("url must start with http:// or https://: {url}"));
    }

    let text = std::fs::read_to_string(file)
        .map_err(|e| format!("cannot read {file}: {e}"))?;

    let version_re = Regex::new(r#"(?m)^(\s*version\s*=\s*")([^"]*)(")"#).unwrap();
    let url_re = Regex::new(r#"(?m)^(\s*url\s*=\s*")([^"]*)(")"#).unwrap();
    let hash_re = Regex::new(r#"(?m)^(\s*hash\s*=\s*")([^"]*)(")"#).unwrap();
    let fetchurl_re = Regex::new(r"fetchurl\s*\{").unwrap();

    let version_caps = version_re
        .captures(&text)
        .ok_or("no version attribute found")?;
    let old_version = version_caps[2].to_string();

    if fetchurl_re.find(&text).is_none() {
        return Err("no pkgs.fetchurl block found".into());
    }

    let fetchurl_at = text.find("fetchurl").unwrap();
    if !url_re
        .captures(&text)
        .is_some_and(|c| c.get(0).unwrap().start() > fetchurl_at)
    {
        return Err("no url attribute found after pkgs.fetchurl".into());
    }
    if !hash_re
        .captures(&text)
        .is_some_and(|c| c.get(0).unwrap().start() > fetchurl_at)
    {
        return Err("no hash attribute found after pkgs.fetchurl".into());
    }

    let multiple = fetchurl_re.find_iter(&text).count() > 1;

    let new_hash = prefetch_hash(url)?;

    let mut new_text = version_re
        .replacen(&text, 1, |c: &regex::Captures| {
            format!("{}{}{}", &c[1], version, &c[3])
        })
        .into_owned();
    new_text = url_re
        .replacen(&new_text, 1, |c: &regex::Captures| {
            format!("{}{}{}", &c[1], url, &c[3])
        })
        .into_owned();
    new_text = hash_re
        .replacen(&new_text, 1, |c: &regex::Captures| {
            format!("{}{}{}", &c[1], new_hash, &c[3])
        })
        .into_owned();

    let final_text = fmt_text(file, &new_text);

    println!("file: {file}");
    println!("version: {old_version} -> {version}");
    println!("url: {url}");
    println!("hash: {new_hash}");
    if multiple {
        println!("warning: multiple matches, only first updated");
    }

    let color = io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none();
    let diff = unified_diff(&text, &final_text, file, color);
    if diff.is_empty() {
        println!("no changes");
        return Ok(());
    }
    print!("{diff}");

    if !confirm("apply patch?") {
        println!("aborted, {file} unchanged");
        return Ok(());
    }

    if backup {
        let bak = Path::new(file).with_extension("nix.bak");
        std::fs::copy(file, &bak).map_err(|e| format!("cannot write backup {bak:?}: {e}"))?;
    }
    std::fs::write(file, final_text.as_bytes()).map_err(|e| format!("cannot write {file}: {e}"))?;
    println!("written: {file}");
    Ok(())
}

/// 用目标文件所在项目 flake 的 formatter 格式化（`nix fmt`），与项目既有风格一致。
/// 失败时原样返回未格式化的文本。
fn fmt_text(file: &str, text: &str) -> String {
    let dir = Path::new(file).parent().filter(|p| !p.as_os_str().is_empty());
    let tmp = std::env::temp_dir().join(format!("nixbump-{}.nix", process::id()));
    if let Err(e) = std::fs::write(&tmp, text) {
        println!("warning: fmt skipped: cannot write temp file: {e}");
        return text.to_string();
    }

    let mut cmd = Command::new("nix");
    cmd.args(["fmt", "--quiet"]).arg(&tmp);
    if let Some(dir) = dir {
        cmd.current_dir(dir);
    }
    let result = match cmd.output() {
        Ok(o) if o.status.success() => std::fs::read_to_string(&tmp).unwrap_or_else(|_| text.to_string()),
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr);
            let stderr = stderr.trim();
            println!(
                "warning: nix fmt failed (exit {}): {stderr}",
                o.status.code().map(|c| c.to_string()).unwrap_or_else(|| "signal".into())
            );
            text.to_string()
        }
        Err(e) => {
            println!("warning: cannot run nix fmt: {e}");
            text.to_string()
        }
    };
    let _ = std::fs::remove_file(&tmp);
    if result != text {
        println!("fmt: ok");
    }
    result
}

fn prefetch_hash(url: &str) -> Result<String, String> {
    eprintln!("prefetch: {url}");
    let child = Command::new("nix")
        .args(["store", "prefetch-file", "--json", "--hash-type", "sha256", url])
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| format!("failed to run nix store prefetch-file: {e}"))?;
    let output = child
        .wait_with_output()
        .map_err(|e| format!("nix store prefetch-file wait failed: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "nix store prefetch-file failed (exit {})",
            output.status.code().map(|c| c.to_string()).unwrap_or_else(|| "signal".into())
        ));
    }
    let json: serde_json::Value = serde_json::from_slice(&output.stdout)
        .map_err(|e| format!("cannot parse prefetch output: {e}"))?;
    json.get("hash")
        .and_then(|h| h.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| "prefetch output missing hash field".into())
}

enum DiffTag {
    Equal,
    Delete,
    Insert,
}

/// 返回 git 风格 unified diff；无改动时返回空串。
fn unified_diff(old: &str, new: &str, path: &str, color: bool) -> String {
    let a: Vec<&str> = old.lines().collect();
    let b: Vec<&str> = new.lines().collect();
    let n = a.len();
    let m = b.len();

    let mut ops: Vec<(DiffTag, usize)> = Vec::new();
    if n.saturating_mul(m) > 4_000_000 {
        for x in 0..n {
            ops.push((DiffTag::Delete, x));
        }
        for y in 0..m {
            ops.push((DiffTag::Insert, y));
        }
    } else {
        let w = m + 1;
        let mut dp = vec![0u32; (n + 1) * w];
        for x in (0..n).rev() {
            for y in (0..m).rev() {
                dp[x * w + y] = if a[x] == b[y] {
                    1 + dp[(x + 1) * w + y + 1]
                } else {
                    dp[(x + 1) * w + y].max(dp[x * w + y + 1])
                };
            }
        }
        let (mut x, mut y) = (0usize, 0usize);
        while x < n && y < m {
            if a[x] == b[y] {
                ops.push((DiffTag::Equal, x));
                x += 1;
                y += 1;
            } else if dp[(x + 1) * w + y] >= dp[x * w + y + 1] {
                ops.push((DiffTag::Delete, x));
                x += 1;
            } else {
                ops.push((DiffTag::Insert, y));
                y += 1;
            }
        }
        while x < n {
            ops.push((DiffTag::Delete, x));
            x += 1;
        }
        while y < m {
            ops.push((DiffTag::Insert, y));
            y += 1;
        }
    }

    let changes: Vec<usize> = ops
        .iter()
        .enumerate()
        .filter(|(_, (t, _))| !matches!(t, DiffTag::Equal))
        .map(|(i, _)| i)
        .collect();
    if changes.is_empty() {
        return String::new();
    }

    let mut hunks: Vec<(usize, usize)> = Vec::new();
    let mut start = changes[0];
    let mut last = changes[0];
    for &c in &changes[1..] {
        if c - last <= 6 {
            last = c;
        } else {
            hunks.push((start, last));
            start = c;
            last = c;
        }
    }
    hunks.push((start, last));

    let cyan = |s: &str| -> String {
        if color {
            format!("\x1b[36m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    };
    let paint = |tag: char, line: &str| -> String {
        match (color, tag) {
            (true, '-') => format!("\x1b[31m-{line}\x1b[0m"),
            (true, '+') => format!("\x1b[32m+{line}\x1b[0m"),
            _ => format!("{tag}{line}"),
        }
    };

    let mut out = String::new();
    let p = path.strip_prefix('/').unwrap_or(path);
    out.push_str(&cyan(&format!("diff --git a/{p} b/{p}\n")));
    out.push_str(&cyan(&format!("--- a/{p}\n")));
    out.push_str(&cyan(&format!("+++ b/{p}\n")));

    for (h_start, h_end) in hunks {
        let lo = h_start.saturating_sub(3);
        let hi = (h_end + 4).min(ops.len());

        let old_before: usize = ops[..lo]
            .iter()
            .filter(|(t, _)| matches!(t, DiffTag::Equal | DiffTag::Delete))
            .count();
        let new_before: usize = ops[..lo]
            .iter()
            .filter(|(t, _)| matches!(t, DiffTag::Equal | DiffTag::Insert))
            .count();
        let old_count: usize = ops[lo..hi]
            .iter()
            .filter(|(t, _)| matches!(t, DiffTag::Equal | DiffTag::Delete))
            .count();
        let new_count: usize = ops[lo..hi]
            .iter()
            .filter(|(t, _)| matches!(t, DiffTag::Equal | DiffTag::Insert))
            .count();

        let mut old_start = old_before + 1;
        let mut new_start = new_before + 1;
        if old_count == 0 {
            old_start -= 1;
        }
        if new_count == 0 {
            new_start -= 1;
        }

        out.push_str(&cyan(&format!(
            "@@ -{old_start},{old_count} +{new_start},{new_count} @@\n"
        )));

        for (tag, idx) in &ops[lo..hi] {
            match tag {
                DiffTag::Equal => out.push_str(&paint(' ', a[*idx])),
                DiffTag::Delete => out.push_str(&paint('-', a[*idx])),
                DiffTag::Insert => out.push_str(&paint('+', b[*idx])),
            }
            out.push('\n');
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::unified_diff;

    #[test]
    fn single_line_change() {
        let d = unified_diff("a\nb\nc\nd\ne\n", "a\nB\nc\nd\ne\n", "f", false);
        assert_eq!(
            d,
            "diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -1,5 +1,5 @@\n a\n-b\n+B\n c\n d\n e\n"
        );
    }

    #[test]
    fn identical_yields_empty() {
        assert_eq!(unified_diff("a\n", "a\n", "f", false), "");
    }
}
