//! Each test names one thing the parse must get right for a package to be
//! pinned to the version that was approved.
use super::*;

fn args(words: &[&str]) -> Vec<String> {
    words.iter().map(|word| (*word).to_owned()).collect()
}

/// The shape every MCP server in the wild is published as.
#[test]
fn the_ordinary_form_gives_the_package_and_the_servers_own_arguments() {
    let held = request(
        "npx",
        &args(&["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]),
    )
    .expect("a request");
    assert_eq!(held.manager, Manager::Npm);
    assert_eq!(held.package, "@modelcontextprotocol/server-filesystem");
    assert_eq!(held.version, None);
    assert_eq!(held.arguments, args(&["/tmp"]));
}

/// A scope's `@` is not a version separator. Reading it as one would pin the
/// wrong thing, or refuse a package that is perfectly ordinary.
#[test]
fn a_scope_is_not_a_version() {
    let scoped = request("npx", &args(&["-y", "@upstash/context7-mcp"])).expect("a request");
    assert_eq!(scoped.package, "@upstash/context7-mcp");
    assert_eq!(scoped.version, None);

    let pinned = request("npx", &args(&["-y", "@playwright/mcp@1.2.3"])).expect("a request");
    assert_eq!(pinned.package, "@playwright/mcp");
    assert_eq!(pinned.version.as_deref(), Some("1.2.3"));

    let plain = request("npx", &args(&["-y", "tavily-mcp@0.9.1"])).expect("a request");
    assert_eq!(plain.package, "tavily-mcp");
    assert_eq!(plain.version.as_deref(), Some("0.9.1"));
}

/// `@latest` is a version as written. It is still not a pin — resolving it to
/// the version that exists at that moment is the fetch step's job — but it
/// must not be mistaken for part of the name.
#[test]
fn a_tag_is_read_as_the_version_it_is() {
    let held = request("npx", &args(&["-y", "@playwright/mcp@latest"])).expect("a request");
    assert_eq!(held.package, "@playwright/mcp");
    assert_eq!(held.version.as_deref(), Some("latest"));
}

/// The subcommand spellings that mean "run this package".
#[test]
fn a_subcommand_before_the_package_is_stepped_over() {
    for words in [
        vec!["exec", "-y", "docker-mcp"],
        vec!["dlx", "docker-mcp"],
        vec!["--package", "docker-mcp"],
    ] {
        let held = request("pnpm", &args(&words)).expect("a request");
        assert_eq!(held.package, "docker-mcp", "{words:?}");
    }
}

/// Python-side launchers name a package the same way.
#[test]
fn the_python_launchers_are_read_too() {
    let held = request("uvx", &args(&["mcp-server-git"])).expect("a request");
    assert_eq!(held.manager, Manager::Python);
    assert_eq!(held.package, "mcp-server-git");
}

/// A command that is not a launcher, or that this cannot read with certainty,
/// keeps the refusal. Guessing here would pin something nobody named.
#[test]
fn anything_uncertain_is_left_refused() {
    // Not a launcher at all.
    assert_eq!(request("/usr/local/bin/node", &args(&["server.js"])), None);
    // A flag this does not know: it may change what runs.
    assert_eq!(
        request("npx", &args(&["--node-options=--inspect", "pkg"])),
        None
    );
    // Nothing after the launcher.
    assert_eq!(request("npx", &args(&["-y"])), None);
    assert_eq!(request("npx", &[]), None);
    // A path, which ordinary registration already pins better.
    assert_eq!(request("npx", &args(&["./local/server.js"])), None);
    // Docker is a launcher, but an image digest is a different kind of pin
    // and is deliberately out of this step's scope.
    assert_eq!(request("docker", &args(&["run", "some/image"])), None);
}

/// The launcher may be written as a full path.
#[test]
fn a_launcher_named_by_path_is_still_a_launcher() {
    let held = request("/opt/homebrew/bin/npx", &args(&["-y", "pkg"])).expect("a request");
    assert_eq!(held.manager, Manager::Npm);
    assert_eq!(held.package, "pkg");
}

fn pin(directory: &std::path::Path, package: &str, version: &str) {
    let held = Pinned {
        schema_version: 1,
        package: package.to_owned(),
        version: version.to_owned(),
        runtime: std::path::PathBuf::from("/usr/local/bin/node"),
        entry: directory.join("node_modules/.bin/server"),
    };
    std::fs::write(
        directory.join("pinned.json"),
        serde_json::to_vec(&held).unwrap(),
    )
    .unwrap();
}

/// A pin answers the command that asked for it.
#[test]
fn a_pin_for_the_package_asked_for_is_the_answer() {
    let dir = tempfile::tempdir().unwrap();
    pin(dir.path(), "@playwright/mcp", "1.2.3");
    let exact = request("npx", &args(&["-y", "@playwright/mcp@1.2.3"])).unwrap();
    assert!(pinned(dir.path(), &exact).is_some());
    // No version named: the pin stands, and its version is what approval shows.
    let any = request("npx", &args(&["-y", "@playwright/mcp"])).unwrap();
    assert_eq!(pinned(dir.path(), &any).unwrap().version, "1.2.3");
    // `latest` is a request to take whatever the pin has, not a version.
    let latest = request("npx", &args(&["-y", "@playwright/mcp@latest"])).unwrap();
    assert!(pinned(dir.path(), &latest).is_some());
}

/// Silently running something other than what was named is the failure this
/// path exists to prevent.
#[test]
fn a_pin_for_anything_else_is_not_an_answer() {
    let dir = tempfile::tempdir().unwrap();
    pin(dir.path(), "@playwright/mcp", "1.2.3");
    let other_version = request("npx", &args(&["-y", "@playwright/mcp@9.9.9"])).unwrap();
    assert_eq!(pinned(dir.path(), &other_version), None);
    let other_package = request("npx", &args(&["-y", "tavily-mcp"])).unwrap();
    assert_eq!(pinned(dir.path(), &other_package), None);
    // Nothing pinned at all.
    let empty = tempfile::tempdir().unwrap();
    let any = request("npx", &args(&["-y", "@playwright/mcp"])).unwrap();
    assert_eq!(pinned(empty.path(), &any), None);
}

/// An entry outside the directory polaris owns would let a pin point at
/// anything on the machine.
#[test]
fn an_entry_outside_the_servers_own_directory_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let held = Pinned {
        schema_version: 1,
        package: "pkg".to_owned(),
        version: "1.0.0".to_owned(),
        runtime: std::path::PathBuf::from("/usr/local/bin/node"),
        entry: std::path::PathBuf::from("/usr/bin/anything"),
    };
    std::fs::write(
        dir.path().join("pinned.json"),
        serde_json::to_vec(&held).unwrap(),
    )
    .unwrap();
    let any = request("npx", &args(&["-y", "pkg"])).unwrap();
    assert_eq!(pinned(dir.path(), &any), None);
}

/// What the one-time fetch runs, and the flags that make it safe to run.
#[test]
fn the_fetch_pins_the_version_and_refuses_install_hooks() {
    let directory = std::path::Path::new("/tmp/server");
    let runtime = std::path::Path::new("/tmp/runtime/bin/node");
    let pinned = request("npx", &args(&["-y", "@scope/pkg@1.2.3"])).unwrap();
    let (program, argv) = install_command(&pinned, directory, runtime).unwrap();
    assert_eq!(program, "npm");
    // The hooks run arbitrary code at fetch time and nobody approved that.
    assert!(argv.contains(&"--ignore-scripts".to_owned()), "{argv:?}");
    // The `.bin` shortcuts npm would write are symbolic links, and they land
    // in the server's own directory — a registered workspace, where a link is
    // refused outright. Nothing needs them: the pin names the program by
    // absolute path.
    assert!(argv.contains(&"--no-bin-links".to_owned()), "{argv:?}");
    // Everything lands where polaris can account for it.
    assert!(argv.contains(&"--prefix".to_owned()));
    assert!(argv.contains(&"/tmp/server".to_owned()));
    assert!(argv.contains(&"@scope/pkg@1.2.3".to_owned()));

    // No version named: the fetch resolves one and the pin records it.
    let loose = request("npx", &args(&["-y", "@scope/pkg"])).unwrap();
    let (_, argv) = install_command(&loose, directory, runtime).unwrap();
    assert!(argv.contains(&"@scope/pkg".to_owned()), "{argv:?}");

    // PyPI takes the same shape through `uv`: one directory polaris owns, no
    // code run at install time, and the interpreter fixed before the fetch so
    // a wheel carrying compiled code is built for the one that will run it.
    let python = request("uvx", &args(&["mcp-server-git==0.6.2"])).unwrap();
    let (program, argv) = install_command(&python, directory, runtime).unwrap();
    assert_eq!(program, "uv");
    // A source distribution builds itself at install time; a wheel is already
    // built. This is the Python counterpart of `--ignore-scripts`.
    assert!(argv.contains(&"--only-binary=:all:".to_owned()), "{argv:?}");
    assert!(argv.contains(&"--target".to_owned()), "{argv:?}");
    assert!(argv.contains(&"/tmp/server/site".to_owned()), "{argv:?}");
    assert!(argv.contains(&"--python".to_owned()), "{argv:?}");
    assert!(
        argv.contains(&"/tmp/runtime/bin/node".to_owned()),
        "the interpreter the pin will name is the one the wheel is built for: {argv:?}"
    );
    // PyPI's separator, not npm's.
    assert!(
        argv.contains(&"mcp-server-git==0.6.2".to_owned()),
        "{argv:?}"
    );
}

fn install(directory: &std::path::Path, package: &str, manifest: serde_json::Value, bin: &str) {
    let root = directory.join("node_modules").join(package);
    let target = root.join(bin);
    std::fs::create_dir_all(target.parent().unwrap()).unwrap();
    std::fs::write(&target, "#!/usr/bin/env node\n").unwrap();
    std::fs::write(
        root.join("package.json"),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();
}

/// The package's own manifest knows where its program is. Guessing at
/// `index.js` would run the wrong file for a package that has both.
#[test]
fn the_entry_point_comes_from_the_packages_own_manifest() {
    let dir = tempfile::tempdir().unwrap();
    // A single `bin` string.
    install(
        dir.path(),
        "@scope/pkg",
        serde_json::json!({"version": "1.2.3", "bin": "dist/cli.js"}),
        "dist/cli.js",
    );
    let held = request("npx", &args(&["-y", "@scope/pkg"])).unwrap();
    let (entry, version) = entry_point(dir.path(), &held).unwrap();
    assert!(entry.ends_with("dist/cli.js"), "{entry:?}");
    assert_eq!(version, "1.2.3");

    // A `bin` map: the one named after the package is the server.
    let other = tempfile::tempdir().unwrap();
    install(
        other.path(),
        "@scope/pkg",
        serde_json::json!({"version": "2.0.0", "bin": {"helper": "h.js", "pkg": "server.js"}}),
        "server.js",
    );
    std::fs::write(
        other.path().join("node_modules/@scope/pkg/h.js"),
        "// helper\n",
    )
    .unwrap();
    let (entry, version) = entry_point(other.path(), &held).unwrap();
    assert!(entry.ends_with("server.js"), "{entry:?}");
    assert_eq!(version, "2.0.0");
}

/// A `bin` that climbs out of the directory polaris owns is not something to
/// launch, whatever the package says.
#[test]
fn an_entry_point_outside_the_directory_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    install(
        dir.path(),
        "pkg",
        serde_json::json!({"version": "1.0.0", "bin": "../../../escape.js"}),
        "server.js",
    );
    std::fs::write(dir.path().parent().unwrap().join("escape.js"), "//\n").ok();
    let held = request("npx", &args(&["-y", "pkg"])).unwrap();
    assert_eq!(entry_point(dir.path(), &held), None);
}

/// Nothing installed, or a manifest without a program, is not an answer.
#[test]
fn a_package_with_no_program_gives_no_entry_point() {
    let dir = tempfile::tempdir().unwrap();
    let held = request("npx", &args(&["-y", "pkg"])).unwrap();
    assert_eq!(entry_point(dir.path(), &held), None);

    install(
        dir.path(),
        "pkg",
        serde_json::json!({"version": "1.0.0"}),
        "server.js",
    );
    assert_eq!(entry_point(dir.path(), &held), None);
}

/// PyPI writes `name==version`, and `uvx` also takes `name@version`. Reading
/// the first as one long package name fetches nothing and cannot say why.
#[test]
fn a_python_launcher_reads_both_spellings_of_a_pinned_version() {
    let equals = request("uvx", &args(&["mcp-server-git==0.6.2", "--repo", "/tmp"])).unwrap();
    assert_eq!(equals.manager, Manager::Python);
    assert_eq!(equals.package, "mcp-server-git");
    assert_eq!(equals.version.as_deref(), Some("0.6.2"));
    assert_eq!(equals.arguments, args(&["--repo", "/tmp"]));

    let at = request("uvx", &args(&["mcp-server-git@0.6.2"])).unwrap();
    assert_eq!(at.package, "mcp-server-git");
    assert_eq!(at.version.as_deref(), Some("0.6.2"));

    // npm's side is untouched: `==` means nothing there, so it stays part of
    // the name and the fetch refuses it rather than fetching something else.
    let npm = request("npx", &args(&["-y", "pkg==1.0.0"])).unwrap();
    assert_eq!(npm.package, "pkg==1.0.0");
}

/// A wheel declares its program as `module:attribute`, never as a file. The
/// declaration is what polaris reads; guessing a module named after the
/// package runs the wrong thing whenever the import name differs from the
/// published one, which on PyPI is ordinary rather than rare.
#[test]
fn the_python_entry_point_comes_from_the_distributions_own_metadata() {
    let temporary = tempfile::tempdir().unwrap();
    let directory = temporary.path();
    let write = |dist: &str, body: &str| {
        let held = directory.join(SITE).join(format!("{dist}.dist-info"));
        std::fs::create_dir_all(&held).unwrap();
        std::fs::write(held.join("entry_points.txt"), body).unwrap();
    };
    // A dependency that also declares a console script, and whose name sorts
    // before the one wanted: reading the first `.dist-info` found is wrong.
    write("click-8.5.0", "[console_scripts]\nclick = click.cli:main\n");
    write(
        "mcp_server_fetch-1.30.0",
        "[pytest11]\nanyio = anyio.pytest_plugin\n\n\
         [console_scripts]\nnot-the-server = other.module:run\nmcp-server-fetch = mcp_server_fetch:main\n\n\
         [distutils.setup_keywords]\ncffi_modules = cffi.setuptools_ext:cffi_modules\n",
    );
    let held = request("uvx", &args(&["mcp-server-fetch"])).unwrap();
    let (module, attribute, version) = python_entry_point(directory, &held).expect("an entry");
    // The script whose name matches the package, not the first one listed.
    assert_eq!(module, "mcp_server_fetch");
    assert_eq!(attribute, "main");
    // The resolved version comes from the directory the installer named.
    assert_eq!(version, "1.30.0");

    // `-`, `_` and `.` are the same character on PyPI, so the spelling in the
    // command need not be the spelling on disk.
    let dotted = request("uvx", &args(&["MCP.Server-Fetch"])).unwrap();
    assert_eq!(
        python_entry_point(directory, &dotted).map(|held| held.0),
        Some("mcp_server_fetch".to_owned())
    );

    // What is read here is written into a file that will be executed, so both
    // halves have to be identifiers. An attribute that is a call is the shape
    // that would carry code across.
    write("evil-1.0", "[console_scripts]\nevil = os:system('id')\n");
    let evil = request("uvx", &args(&["evil"])).unwrap();
    assert_eq!(python_entry_point(directory, &evil), None);
    // And a target with no `:` names no attribute to call.
    write("halved-1.0", "[console_scripts]\nhalved = os.system\n");
    let halved = request("uvx", &args(&["halved"])).unwrap();
    assert_eq!(python_entry_point(directory, &halved), None);

    // A distribution with no console script at all.
    write("quiet-1.0", "[pytest11]\nx = y\n");
    let quiet = request("uvx", &args(&["quiet"])).unwrap();
    assert_eq!(python_entry_point(directory, &quiet), None);
}

/// The one piece of code polaris authors rather than fetches. It imports what
/// the distribution declared and calls it, and nothing else — and it is
/// digested and re-checked before every launch, which the installer's own
/// launcher script is not.
#[test]
fn the_written_entry_imports_what_was_declared_and_calls_it() {
    let source = python_entry_source("mcp_server_fetch", "main");
    assert!(
        source.contains("importlib.import_module(\"mcp_server_fetch\")"),
        "{source}"
    );
    assert!(source.contains("\"main\".split(\".\")"), "{source}");
    assert!(source.contains("sys.exit("), "{source}");
    // A dotted attribute is walked, not imported, so `cli:app.run` works.
    assert!(python_entry_source("pkg", "app.run").contains("\"app.run\".split(\".\")"));
}
