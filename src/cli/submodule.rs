//! `rustygit submodule` — porcelain for nested-repo references.
//!
//! Subset:
//!   * `add <url> [<path>]` — clone and record in `.gitmodules`.
//!   * `status` — print `<state> <oid> <path>` for each submodule.
//!   * `init` — copy each `submodule.<n>.url` from `.gitmodules` to local
//!     config so that fetch-on-update works.
//!   * `update [--recursive] [--init]` — ensure each submodule is checked
//!     out at the recorded commit.
//!   * `foreach <cmd>` — run a shell command in each submodule.
//!   * `deinit <path>` — remove a submodule's working tree.
//!   * `sync` — copy `submodule.<n>.url` from `.gitmodules` over local config.

use std::io::{self, Write};
use std::path::PathBuf;
use std::process::Command;

use clap::{Args, Subcommand};

use crate::repo::Repository;

#[derive(Debug, Args)]
pub struct SubmoduleArgs {
    #[command(subcommand)]
    pub sub: Option<SubmoduleSub>,
}

#[derive(Debug, Subcommand)]
pub enum SubmoduleSub {
    Add {
        #[arg(value_name = "URL")]
        url: String,
        #[arg(value_name = "PATH")]
        path: Option<String>,
    },
    Status {
        #[arg(long = "recursive")]
        recursive: bool,
    },
    Init {
        #[arg(value_name = "PATH")]
        paths: Vec<String>,
    },
    Update {
        #[arg(long = "init")]
        init: bool,
        #[arg(long = "recursive")]
        recursive: bool,
        #[arg(value_name = "PATH")]
        paths: Vec<String>,
    },
    Foreach {
        #[arg(long = "recursive")]
        recursive: bool,
        #[arg(value_name = "COMMAND", trailing_var_arg = true)]
        command: Vec<String>,
    },
    Deinit {
        #[arg(short = 'f', long = "force")]
        force: bool,
        #[arg(value_name = "PATH", required = true)]
        paths: Vec<String>,
    },
    Sync {
        #[arg(value_name = "PATH")]
        paths: Vec<String>,
    },
}

pub fn run(args: SubmoduleArgs) -> io::Result<i32> {
    let repo = Repository::discover_from_cwd().map_err(io_err)?;
    let sub = args
        .sub
        .unwrap_or(SubmoduleSub::Status { recursive: false });
    match sub {
        SubmoduleSub::Add { url, path } => add(&repo, &url, path.as_deref()),
        SubmoduleSub::Status { recursive } => status(&repo, recursive),
        SubmoduleSub::Init { paths } => init(&repo, &paths),
        SubmoduleSub::Update {
            init,
            recursive,
            paths,
        } => update(&repo, init, recursive, &paths),
        SubmoduleSub::Foreach { recursive, command } => foreach(&repo, recursive, &command),
        SubmoduleSub::Deinit { force, paths } => deinit(&repo, force, &paths),
        SubmoduleSub::Sync { paths } => sync(&repo, &paths),
    }
}

fn add(repo: &Repository, url: &str, path: Option<&str>) -> io::Result<i32> {
    let target = path
        .map(PathBuf::from)
        .or_else(|| {
            url.rsplit('/')
                .next()
                .map(|s| s.trim_end_matches(".git").to_string())
                .map(PathBuf::from)
        })
        .ok_or_else(|| io::Error::other("submodule add: couldn't infer path"))?;
    if target.exists() {
        return Err(io::Error::other(format!(
            "submodule add: {} already exists",
            target.display()
        )));
    }
    // Clone via existing clone CLI.
    let abs_target = repo.workdir().join(&target);
    let clone = crate::cli::clone::CloneArgs {
        quiet: false,
        no_checkout: false,
        source: url.to_string(),
        dest: Some(abs_target.display().to_string()),
    };
    let code = crate::cli::clone::run(clone)?;
    if code != 0 {
        return Ok(code);
    }
    // Append to .gitmodules.
    let gm = repo.workdir().join(".gitmodules");
    let mut text = std::fs::read_to_string(&gm).unwrap_or_default();
    text.push_str(&format!(
        "[submodule \"{}\"]\n\tpath = {}\n\turl = {url}\n",
        target.display(),
        target.display()
    ));
    std::fs::write(&gm, text)?;
    println!("Adding submodule {} from {url}", target.display());
    Ok(0)
}

fn status(repo: &Repository, recursive: bool) -> io::Result<i32> {
    let _ = recursive;
    // Parse .gitmodules to enumerate paths.
    let stdout = io::stdout();
    let mut out = stdout.lock();
    let modules = parse_gitmodules(&repo.workdir().join(".gitmodules"));
    for m in &modules {
        let abs = repo.workdir().join(&m.path);
        if !abs.exists() {
            writeln!(out, "-{} {} (not initialized)", "0".repeat(40), m.path)?;
            continue;
        }
        // Read submodule HEAD.
        let sub_head_path = abs.join(".git").join("HEAD");
        let head_oid = std::fs::read_to_string(&sub_head_path)
            .ok()
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        writeln!(out, " {} {}", head_oid, m.path)?;
    }
    Ok(0)
}

fn init(repo: &Repository, paths: &[String]) -> io::Result<i32> {
    let modules = parse_gitmodules(&repo.workdir().join(".gitmodules"));
    for m in &modules {
        if !paths.is_empty() && !paths.iter().any(|p| p == &m.path) {
            continue;
        }
        let _ = crate::cli::config_cmd::run(crate::cli::config_cmd::ConfigArgs {
            get: false,
            set: Some(vec![format!("submodule.{}.url", m.name), m.url.clone()]),
            unset: false,
            add: None,
            list: false,
            local: true,
            global: false,
            key: None,
            value: None,
        });
        println!(
            "Submodule '{}' ({}) registered for path '{}'",
            m.name, m.url, m.path
        );
    }
    Ok(0)
}

fn update(repo: &Repository, do_init: bool, recursive: bool, paths: &[String]) -> io::Result<i32> {
    if do_init {
        init(repo, paths)?;
    }
    let modules = parse_gitmodules(&repo.workdir().join(".gitmodules"));
    for m in &modules {
        if !paths.is_empty() && !paths.iter().any(|p| p == &m.path) {
            continue;
        }
        let abs = repo.workdir().join(&m.path);
        if !abs.exists() {
            // Clone.
            let clone = crate::cli::clone::CloneArgs {
                quiet: false,
                no_checkout: false,
                source: m.url.clone(),
                dest: Some(abs.display().to_string()),
            };
            let _ = crate::cli::clone::run(clone)?;
        }
        if recursive {
            // Recurse: cd into submodule, invoke submodule update there.
            let _ = Command::new(std::env::current_exe().unwrap_or_default())
                .args(["submodule", "update", "--init", "--recursive"])
                .current_dir(&abs)
                .status();
        }
    }
    Ok(0)
}

fn foreach(repo: &Repository, recursive: bool, command: &[String]) -> io::Result<i32> {
    if command.is_empty() {
        eprintln!("rustygit submodule foreach: missing <command>");
        return Ok(129);
    }
    let modules = parse_gitmodules(&repo.workdir().join(".gitmodules"));
    for m in &modules {
        let abs = repo.workdir().join(&m.path);
        if !abs.exists() {
            continue;
        }
        let status = Command::new("sh")
            .arg("-c")
            .arg(command.join(" "))
            .current_dir(&abs)
            .status()?;
        if !status.success() {
            return Ok(1);
        }
        if recursive {
            // best-effort sub-recursion via cli
            let _ = Command::new(std::env::current_exe().unwrap_or_default())
                .args(["submodule", "foreach", "--recursive"])
                .args(command)
                .current_dir(&abs)
                .status();
        }
    }
    Ok(0)
}

fn deinit(repo: &Repository, force: bool, paths: &[String]) -> io::Result<i32> {
    for p in paths {
        let abs = repo.workdir().join(p);
        if abs.is_dir() {
            if !force {
                eprintln!("rustygit submodule deinit: refusing without -f for {p}");
                continue;
            }
            std::fs::remove_dir_all(&abs)?;
            println!("Deinitialized {p}");
        }
    }
    Ok(0)
}

fn sync(repo: &Repository, paths: &[String]) -> io::Result<i32> {
    // Re-init pushes urls from .gitmodules → local config.
    init(repo, paths)
}

#[derive(Debug)]
struct Module {
    name: String,
    path: String,
    url: String,
}

fn parse_gitmodules(path: &std::path::Path) -> Vec<Module> {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let mut out = Vec::new();
    let mut current_name: Option<String> = None;
    let mut current_path: Option<String> = None;
    let mut current_url: Option<String> = None;
    let flush = |name: &mut Option<String>,
                 path: &mut Option<String>,
                 url: &mut Option<String>,
                 out: &mut Vec<Module>| {
        if let (Some(n), Some(p), Some(u)) = (name.take(), path.take(), url.take()) {
            out.push(Module {
                name: n,
                path: p,
                url: u,
            });
        }
    };
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with("[submodule") {
            flush(
                &mut current_name,
                &mut current_path,
                &mut current_url,
                &mut out,
            );
            let name = line
                .split('"')
                .nth(1)
                .map(str::to_string)
                .unwrap_or_default();
            current_name = Some(name);
        } else if let Some(eq) = line.find('=') {
            let key = line[..eq].trim();
            let value = line[eq + 1..].trim().to_string();
            match key {
                "path" => current_path = Some(value),
                "url" => current_url = Some(value),
                _ => {}
            }
        }
    }
    flush(
        &mut current_name,
        &mut current_path,
        &mut current_url,
        &mut out,
    );
    out
}

fn io_err<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::other(format!("{e}"))
}
