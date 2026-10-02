use std::io::{self, Write};
use std::path::Path;
use std::process::{self, Command};

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

    if backup {
        let bak = Path::new(file).with_extension("nix.bak");
        std::fs::copy(file, &bak).map_err(|e| format!("cannot write backup {bak:?}: {e}"))?;
    }
    std::fs::write(file, new_text.as_bytes()).map_err(|e| format!("cannot write {file}: {e}"))?;

    println!("file: {file}");
    println!("version: {old_version} -> {version}");
    println!("url: {url}");
    println!("hash: {new_hash}");
    if multiple {
        println!("warning: multiple matches, only first updated");
    }

    run_fmt(file);
    Ok(())
}

fn run_fmt(file: &str) {
    match Command::new("nixfmt").arg(file).status() {
        Ok(s) if s.success() => println!("fmt: ok"),
        Ok(s) => println!(
            "warning: nixfmt failed: exit {}",
            s.code().map(|c| c.to_string()).unwrap_or_else(|| "signal".into())
        ),
        Err(e) => println!("warning: nixfmt failed: {e}"),
    }
}

fn prefetch_hash(url: &str) -> Result<String, String> {
    match Command::new("nix")
        .args(["store", "prefetch-file", "--json", url])
        .output()
    {
        Ok(o) if o.status.success() => {
            let stdout = String::from_utf8_lossy(&o.stdout);
            let json: serde_json::Value = serde_json::from_str(&stdout)
                .map_err(|e| format!("cannot parse prefetch output: {e}"))?;
            json.get("hash")
                .and_then(|h| h.as_str())
                .map(|s| s.to_string())
                .ok_or_else(|| "prefetch output missing hash field".into())
        }
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr);
            if stderr.contains("unknown command") {
                fallback_hash(url)
            } else {
                Err(format!("nix store prefetch-file failed: {}", tail(&stderr)))
            }
        }
        Err(_) => fallback_hash(url),
    }
}

fn fallback_hash(url: &str) -> Result<String, String> {
    let o = Command::new("nix-prefetch-url")
        .args(["--type", "sha256", url])
        .output()
        .map_err(|e| format!("nix-prefetch-url failed to run: {e}"))?;
    if !o.status.success() {
        return Err(format!(
            "nix-prefetch-url failed: {}",
            tail(&String::from_utf8_lossy(&o.stderr))
        ));
    }
    let hex = String::from_utf8_lossy(&o.stdout).trim().to_string();
    let c = Command::new("nix")
        .args(["hash", "convert", "--hash-algo", "sha256", "--to", "sri", &hex])
        .output()
        .map_err(|e| format!("nix hash convert failed to run: {e}"))?;
    if !c.status.success() {
        return Err(format!(
            "nix hash convert failed: {}",
            tail(&String::from_utf8_lossy(&c.stderr))
        ));
    }
    Ok(String::from_utf8_lossy(&c.stdout).trim().to_string())
}

fn tail(s: &str) -> String {
    s.lines().last().unwrap_or("").to_string()
}
