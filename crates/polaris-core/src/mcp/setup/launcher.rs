//! What a launcher command is actually asking for.
//!
//! `npx -y @scope/pkg@1.2.3 --flag` names a package, a version and the
//! arguments the server itself wants. Registration refuses to run it: a
//! launcher decides what to execute at launch time, so the digest polaris
//! would record is the launcher's, and the code that runs can change between
//! launches with nobody approving it. **That refusal is the guarantee, and it
//! is not being removed here.**
//!
//! What this does is read the request out of the command so the package can be
//! fetched **once**, pinned in polaris's own directory for that server, and
//! launched from there afterwards — at which point it is an ordinary local
//! executable with a digest, which is the shape registration already accepts.
//!
//! Parsing only. Nothing here fetches, writes or executes.

/// The package manager a launcher stands for. Only the ones whose fetch can
/// be pinned to an exact version and installed into a directory polaris owns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Manager {
    /// `npx`, and the package managers that front the same registry.
    Npm,
    /// `uvx`/`uv`, and `pipx`.
    Python,
}

/// What a launcher command was asking to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Request {
    pub(super) manager: Manager,
    /// The package as written, without the version.
    pub package: String,
    /// The version as written, when the command pinned one. `None` means the
    /// command asked for whatever is newest, which is exactly what has to be
    /// resolved and recorded before anything is approved.
    pub(super) version: Option<String>,
    /// What follows the package: the server's own arguments, untouched.
    pub(super) arguments: Vec<String>,
}

/// Flags that take no value, so the word after them is still the package.
/// Anything else that looks like a flag ends the parse: a launcher option
/// this does not know about may change what gets run, and guessing is the
/// one thing a pinning step may not do.
const FLAGS: &[&str] = &["-y", "--yes", "-q", "--quiet", "--silent"];

/// Reads the request out of `command` and `args`, or `None` when the command
/// is not a launcher this can pin.
///
/// `None` is not a failure. It means registration keeps refusing, which is
/// the behaviour that was there before.
pub(super) fn request(command: &str, args: &[String]) -> Option<Request> {
    let manager = match std::path::Path::new(command).file_name()?.to_str()? {
        "npx" | "npm" | "pnpm" | "yarn" | "bunx" => Manager::Npm,
        "uvx" | "pipx" => Manager::Python,
        _ => return None,
    };
    let mut rest = args.iter();
    let package = loop {
        let word = rest.next()?;
        if FLAGS.contains(&word.as_str()) {
            continue;
        }
        // `npm exec` and `pnpm dlx` put a subcommand first; the word after it
        // is the package. Only the spellings that mean "run this package",
        // and checked before the flag rejection because one of them is
        // spelled as a flag.
        if matches!(word.as_str(), "exec" | "dlx" | "run" | "--package") {
            continue;
        }
        if word.starts_with('-') {
            return None;
        }
        break word;
    };
    let (package, version) = match manager {
        // PyPI's own spelling. `uvx` also takes `name@version`, which the
        // shared split below reads, but a config copied from a package's own
        // page is written `name==version` — and reading that as a package
        // named `name==version` fetches nothing and then cannot say why.
        Manager::Python => match package.split_once("==") {
            Some((name, version)) if !name.is_empty() && !version.is_empty() => {
                (name.to_owned(), Some(version.to_owned()))
            }
            _ => split_version(package)?,
        },
        Manager::Npm => split_version(package)?,
    };
    Some(Request {
        manager,
        package,
        version,
        arguments: rest.cloned().collect(),
    })
}

/// Splits `@scope/name@1.2.3` into its name and version.
///
/// The leading `@` of a scope is not a version separator, which is the whole
/// difficulty: `@scope/name` has two `@` and one of them means nothing here.
fn split_version(word: &str) -> Option<(String, Option<String>)> {
    if word.is_empty() || word.contains(['/', '\\'].as_ref()) && !word.starts_with('@') {
        // A path, not a package name. Pinning a path is what the ordinary
        // registration already does, and better.
        return None;
    }
    let scoped = word.starts_with('@');
    let body = if scoped { &word[1..] } else { word };
    match body.rsplit_once('@') {
        Some((name, version)) if !name.is_empty() && !version.is_empty() => Some((
            if scoped {
                format!("@{name}")
            } else {
                name.to_owned()
            },
            Some(version.to_owned()),
        )),
        _ => Some((word.to_owned(), None)),
    }
}

#[cfg(test)]
#[path = "launcher/tests.rs"]
mod tests;

/// What polaris fetched once and now launches instead of the launcher.
///
/// Written by the step that fetches; read here. It names the runtime and the
/// entry point as paths, so from this point on registration treats the server
/// exactly like any other local executable: same shebang handling, same
/// digest, same install root, same approval text. **Nothing about the
/// guarantee is special-cased for a package that arrived this way.**
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Pinned {
    pub schema_version: u32,
    /// The package this was fetched for, and the exact version that was
    /// resolved at that moment. A request for a different package, or for a
    /// version other than the one pinned, is not this pin.
    pub package: String,
    pub version: String,
    /// The interpreter to run, as an absolute path.
    pub runtime: std::path::PathBuf,
    /// The file it runs, as an absolute path inside the server's directory.
    pub entry: std::path::PathBuf,
}

/// The pin kept for `directory`, when there is one that answers `request`.
///
/// A pin for another package, or for a version the command did not ask for,
/// is not an answer: the command named what it wanted and silently running
/// something else is the failure this whole path exists to prevent. A command
/// that named no version accepts the pin as it stands — that is what "fetch
/// once and keep using it" means, and the version is shown at approval.
pub(super) fn pinned(directory: &std::path::Path, request: &Request) -> Option<Pinned> {
    let bytes = std::fs::read(directory.join("pinned.json")).ok()?;
    let held: Pinned = serde_json::from_slice(&bytes).ok()?;
    if held.schema_version != 1 || held.package != request.package {
        return None;
    }
    match &request.version {
        // `latest` is not a version anyone can hold to; it is a request to
        // take whatever the pin has.
        Some(wanted) if wanted != "latest" && *wanted != held.version => None,
        _ => (held.runtime.is_absolute() && held.entry.starts_with(directory)).then_some(held),
    }
}

/// Where a Python package and its dependencies are put: one flat directory
/// inside the one polaris owns for this server.
///
/// Flat matters. Python puts a script's own directory at the front of the
/// import path, so a program placed here imports everything beside it with no
/// virtual environment, no `PYTHONPATH` and no activation — which is what lets
/// the pin be a plain file handed to a plain interpreter, the same shape npm's
/// side already has.
pub(super) const SITE: &str = "site";

/// The program polaris writes for a Python package, beside what it imports.
///
/// A wheel declares its program as `module:function`, not as a file, and the
/// launcher scripts the installer writes sit in a `bin` directory whose own
/// location is not on the import path. Neither is something a digest can be
/// taken of and re-checked. So the declared entry point is written out as a
/// file here, once, and from then on it is an ordinary pinned program.
pub(super) const PYTHON_ENTRY: &str = "polaris_entry.py";

/// The one-time fetch, as a command.
///
/// Separated from running it so the decision is testable without a network:
/// what gets run, with which flags, into which directory, is the part that
/// has to be right.
///
/// - `--ignore-scripts`, because a package's install hooks run arbitrary code
///   at fetch time and nobody approved that. A package that genuinely needs
///   them is a separate, stated decision, not a default.
/// - `--prefix`, so everything lands inside the directory polaris owns for
///   this server and nothing reaches a shared `node_modules`.
/// - `--no-bin-links`, because the `node_modules/.bin` shortcuts npm would
///   write are symbolic links, and the server's own directory is a registered
///   workspace where a link is refused — a link is a path out of the tree, and
///   admission cannot tell a convenient one from an escape. Observed on
///   2026-09-18: 78 of them across 24 servers, and every server refused for
///   them. Nothing needs them: the pin names the program by absolute path.
/// - The exact version, when the command named one. `latest` is resolved by
///   the fetch and recorded afterwards; that resolved value is what approval
///   shows and what the pin holds.
pub(super) fn install_command(
    request: &Request,
    directory: &std::path::Path,
    runtime: &std::path::Path,
) -> Option<(String, Vec<String>)> {
    match request.manager {
        Manager::Npm => {
            let spec = match &request.version {
                Some(version) => format!("{}@{version}", request.package),
                None => request.package.clone(),
            };
            Some((
                "npm".to_owned(),
                vec![
                    "install".to_owned(),
                    "--prefix".to_owned(),
                    directory.to_str()?.to_owned(),
                    "--ignore-scripts".to_owned(),
                    "--no-bin-links".to_owned(),
                    "--no-audit".to_owned(),
                    "--no-fund".to_owned(),
                    "--no-package-lock".to_owned(),
                    spec,
                ],
            ))
        }
        // `uv` rather than `pip`, because it is the tool the launchers this
        // replaces (`uvx`, and `pipx` in practice) already are, and because it
        // takes the interpreter as an argument instead of being one.
        //
        // - `--target`, the Python counterpart of npm's `--prefix`: everything
        //   lands in one directory polaris owns, flat, with nothing reaching a
        //   shared site-packages.
        // - `--only-binary=:all:`, which is the counterpart of
        //   `--ignore-scripts`. A source distribution runs its own build at
        //   install time and nobody approved that; a wheel is already built.
        //   A package published without a wheel is refused rather than built.
        // - `--python`, because a wheel carrying compiled code is built for one
        //   interpreter's ABI. The interpreter is decided here, before the
        //   fetch, and it is the same one the pin will name.
        Manager::Python => {
            let spec = match &request.version {
                Some(version) => format!("{}=={version}", request.package),
                None => request.package.clone(),
            };
            Some((
                "uv".to_owned(),
                vec![
                    "pip".to_owned(),
                    "install".to_owned(),
                    "--target".to_owned(),
                    directory.join(SITE).to_str()?.to_owned(),
                    "--only-binary=:all:".to_owned(),
                    "--python".to_owned(),
                    runtime.to_str()?.to_owned(),
                    spec,
                ],
            ))
        }
    }
}

/// The `module:attribute` a wheel declares as its program, and the version.
///
/// Read from the installed distribution's own metadata: `entry_points.txt`
/// names the console scripts, and the `.dist-info` directory name carries the
/// version that was actually resolved. The script whose name matches the
/// package is the server; a lone script is taken as it stands. Guessing at a
/// module named after the package would run the wrong thing for a
/// distribution whose import name differs from its published name, which is
/// common enough on PyPI to be the normal case rather than the exception.
fn python_entry_point(
    directory: &std::path::Path,
    request: &Request,
) -> Option<(String, String, String)> {
    // PyPI treats `-`, `_` and `.` as the same character and folds case.
    let normalized = |name: &str| name.to_lowercase().replace(['-', '.'], "_");
    let wanted = normalized(&request.package);
    let site = directory.join(SITE);
    let (name, version, body) = std::fs::read_dir(&site).ok()?.flatten().find_map(|held| {
        let file_name = held.file_name();
        let directory_name = file_name.to_str()?.strip_suffix(".dist-info")?;
        let (name, version) = directory_name.rsplit_once('-')?;
        (normalized(name) == wanted).then(|| {
            (
                name.to_owned(),
                version.to_owned(),
                std::fs::read_to_string(held.path().join("entry_points.txt")).unwrap_or_default(),
            )
        })
    })?;
    let mut scripts = body
        .lines()
        .skip_while(|line| line.trim() != "[console_scripts]")
        .skip(1)
        .take_while(|line| !line.trim_start().starts_with('['))
        .filter_map(|line| line.split_once('='))
        .map(|(script, target)| (script.trim().to_owned(), target.trim().to_owned()))
        .collect::<Vec<_>>();
    let chosen = scripts
        .iter()
        .position(|(script, _)| normalized(script) == normalized(&name))
        .unwrap_or(0);
    let (_, target) = (chosen < scripts.len()).then(|| scripts.swap_remove(chosen))?;
    // `module:attribute`, where the attribute may be dotted. Anything else is
    // not something to write into a file that will be executed.
    let (module, attribute) = target.split_once(':')?;
    let identifier = |text: &str| {
        !text.is_empty()
            && text
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'.')
    };
    (identifier(module) && identifier(attribute))
        .then(|| (module.to_owned(), attribute.to_owned(), version))
}

/// Where the fetched package says its program is.
///
/// Read from the package's own `package.json`, because that is the file that
/// knows: a `bin` string names the one program, and a `bin` map names several
/// of which the one matching the package's own name is the server. Guessing
/// at `index.js` would run the wrong file for a package that has both.
pub(super) fn entry_point(
    directory: &std::path::Path,
    request: &Request,
) -> Option<(std::path::PathBuf, String)> {
    let installed = directory.join("node_modules").join(&request.package);
    let manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(installed.join("package.json")).ok()?).ok()?;
    let version = manifest.get("version")?.as_str()?.to_owned();
    let short = request
        .package
        .rsplit_once('/')
        .map_or(request.package.as_str(), |(_, name)| name);
    let relative = match manifest.get("bin")? {
        serde_json::Value::String(only) => only.as_str(),
        serde_json::Value::Object(named) => named
            .get(short)
            .or_else(|| named.values().next())
            .and_then(serde_json::Value::as_str)?,
        _ => return None,
    };
    let entry = installed.join(relative);
    // The pin must name a file inside the directory polaris owns. A `bin`
    // that climbs out of it is not something to launch.
    let entry = entry.canonicalize().ok()?;
    let owned = directory.canonicalize().ok()?;
    entry.starts_with(&owned).then_some((entry, version))
}

/// The interpreter to pin when the caller named none.
///
/// Node is looked for at fixed absolute paths. Python is asked of `uv`, and
/// only among the interpreters `uv` manages itself: those are self-contained,
/// owned by the person running them, and writable by nobody else — which is
/// what a readable root has to be. A system Python is often none of those. The
/// one on this machine, installed from python.org, has 9540 group-writable
/// entries under it, and a tree anyone in a group can rewrite is not a tree to
/// grant a server read of.
///
/// `--resolve-links`, because `uv` points at its current version through a
/// symbolic link and a pinned file has to be named by its real path.
///
/// `None` when none is found. Nothing is installed to make one appear:
/// fetching an interpreter is a larger thing than fetching a package and is
/// the person's decision, not a side effect of this one.
#[cfg(unix)]
pub(super) fn default_runtime(manager: Manager) -> Option<std::path::PathBuf> {
    match manager {
        Manager::Npm => [
            "/usr/local/bin/node",
            "/opt/homebrew/bin/node",
            "/usr/bin/node",
        ]
        .into_iter()
        .map(std::path::PathBuf::from)
        .find(|held| held.is_file()),
        Manager::Python => {
            let found = std::process::Command::new("uv")
                .args([
                    "python",
                    "find",
                    "--resolve-links",
                    "--managed-python",
                    ">=3.10",
                ])
                .stdin(std::process::Stdio::null())
                .output()
                .ok()?;
            found.status.success().then(|| {
                std::path::PathBuf::from(String::from_utf8_lossy(&found.stdout).trim().to_owned())
            })
        }
    }
}

/// The program polaris writes for `module:attribute`.
///
/// This is the one piece of code polaris authors rather than fetches, and it
/// is kept to what the installer's own launcher scripts do: import what the
/// distribution declared and call it. It is digested like any other pinned
/// file and re-checked before every launch, so unlike the installer's script
/// it cannot be changed underneath without the launch failing.
///
/// `module` and `attribute` are checked to be identifiers before they reach
/// here; the attribute is walked rather than imported so a dotted one works.
fn python_entry_source(module: &str, attribute: &str) -> String {
    format!(
        "# polarisが書いた。{module}:{attribute}を呼ぶだけで、ほかは何もしない。\n\
         import importlib, sys\n\
         _held = importlib.import_module(\"{module}\")\n\
         for _part in \"{attribute}\".split(\".\"):\n\
         \x20   _held = getattr(_held, _part)\n\
         sys.exit(_held())\n"
    )
}

/// What a fetch could not do, in the terms the person is told.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refused {
    /// The command is not a launcher this can pin.
    NotALauncher,
    /// A launcher whose package manager this does not install.
    ManagerNotSupported,
    /// The fetch itself failed: no network, no such package, a refusal.
    FetchFailed,
    /// It fetched, but the package does not say where its program is, or
    /// says somewhere outside the directory polaris owns.
    NoEntryPoint,
    /// The runtime that would run it could not be pinned.
    RuntimeNotPinned,
    /// The pin could not be written.
    NotSaved,
}

/// Fetches `command`/`args` once into `directory` and writes the pin.
///
/// **This is the one place polaris runs a package manager**, and it runs it
/// exactly once per registration rather than once per launch. That is the
/// whole difference: what a launcher would decide at every start is decided
/// here, in the open, and then held still. The caller has already taken the
/// person's approval for reaching the network.
///
/// `runtime` is the interpreter. `None` takes the default for the manager the
/// command names — which cannot be decided before the command is read, since
/// a node default is no use to a Python package. Either way the `PATH` is not
/// searched: a version manager rewrites it, and the interpreter that resolves
/// through one is not the interpreter that will be there at launch.
#[cfg(unix)]
pub fn materialise(
    directory: &std::path::Path,
    command: &str,
    args: &[String],
    runtime: Option<&std::path::Path>,
) -> Result<Pinned, Refused> {
    materialise_with_output(directory, command, args, runtime, false)
}

/// Uses the same fetch and pin checks without writing untrusted package output
/// into an interactive renderer. Registration still requires separate approval.
#[cfg(unix)]
pub fn materialise_quiet(
    directory: &std::path::Path,
    command: &str,
    args: &[String],
    runtime: Option<&std::path::Path>,
) -> Result<Pinned, Refused> {
    materialise_with_output(directory, command, args, runtime, true)
}

#[cfg(unix)]
fn materialise_with_output(
    directory: &std::path::Path,
    command: &str,
    args: &[String],
    runtime: Option<&std::path::Path>,
    quiet: bool,
) -> Result<Pinned, Refused> {
    let request = request(command, args).ok_or(Refused::NotALauncher)?;
    let chosen;
    let runtime = match runtime {
        Some(held) => held,
        None => {
            chosen = default_runtime(request.manager).ok_or(Refused::RuntimeNotPinned)?;
            &chosen
        }
    };
    if !runtime.is_absolute() || !runtime.is_file() {
        return Err(Refused::RuntimeNotPinned);
    }
    let (program, argv) =
        install_command(&request, directory, runtime).ok_or(Refused::ManagerNotSupported)?;
    // 0o700, matching what the store creates for everything else it owns.
    // `create_dir_all` leaves the process umask in charge, which on this
    // machine gives 0o755 — and registration then refuses the whole server
    // because polaris keeps its own tree unreadable by others. Observed on
    // 2026-09-18: 24 servers fetched and every one of them refused with
    // `unsafe_location`.
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(directory)
            .map_err(|_| Refused::NotSaved)?;
    }
    let mut fetch = std::process::Command::new(&program);
    fetch
        .args(&argv)
        // A fetch writes into the directory and nowhere else. It is given no
        // inherited stdin so a package cannot prompt its way into running.
        .stdin(std::process::Stdio::null());
    if quiet {
        fetch
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
    }
    let status = fetch.status().map_err(|_| Refused::FetchFailed)?;
    if !status.success() {
        return Err(Refused::FetchFailed);
    }
    let (entry, version) = match request.manager {
        Manager::Npm => entry_point(directory, &request).ok_or(Refused::NoEntryPoint)?,
        Manager::Python => {
            // `uv` leaves its install lock behind, mode 0666 — a file anyone
            // may write. The directory becomes the server's workspace, and
            // admission refuses a workspace holding a file others can write:
            // whoever can write it can put something there the server will
            // read. The lock has no use once the install has finished, so it
            // goes rather than being made narrower. Observed on 2026-09-18:
            // every Python server refused for this one empty file.
            let _ = std::fs::remove_file(directory.join(SITE).join(".lock"));
            let (module, attribute, version) =
                python_entry_point(directory, &request).ok_or(Refused::NoEntryPoint)?;
            let entry = directory.join(SITE).join(PYTHON_ENTRY);
            std::fs::write(&entry, python_entry_source(&module, &attribute))
                .map_err(|_| Refused::NotSaved)?;
            (
                entry.canonicalize().map_err(|_| Refused::NotSaved)?,
                version,
            )
        }
    };
    let held = Pinned {
        schema_version: 1,
        package: request.package,
        version,
        runtime: runtime.to_owned(),
        entry,
    };
    // Written whole, then moved into place: a reader must never see a
    // half-written pin and launch from it.
    let path = directory.join("pinned.json");
    let temporary = directory.join("pinned.json.partial");
    std::fs::write(
        &temporary,
        serde_json::to_vec_pretty(&held).map_err(|_| Refused::NotSaved)?,
    )
    .map_err(|_| Refused::NotSaved)?;
    std::fs::rename(&temporary, &path).map_err(|_| Refused::NotSaved)?;
    Ok(held)
}

#[cfg(not(unix))]
pub fn materialise(
    _directory: &std::path::Path,
    _command: &str,
    _args: &[String],
    _runtime: Option<&std::path::Path>,
) -> Result<Pinned, Refused> {
    Err(Refused::ManagerNotSupported)
}
