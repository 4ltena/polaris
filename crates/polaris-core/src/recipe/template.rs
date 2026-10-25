//! M7c-3e: the files the configure step writes.
//!
//! These are fixed text that Polaris ships, not something a model composes.
//! The step writes each one only where nothing of that name is there, so a
//! project that already has a configuration keeps it: overwriting someone's
//! own settings is not "setting up", and a recipe that quietly replaces them
//! is worse than one that stops.
use std::path::Path;

/// One file the configure step would write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Template {
    pub path: &'static str,
    pub body: &'static str,
}

/// What `nextjs_setting` writes. Deliberately small: the least that makes a
/// Next.js project build, with nothing chosen on the person's behalf that
/// they would have to undo.
pub const NEXT_JS: &[Template] = &[
    Template {
        path: "next.config.mjs",
        body: "/** @type {import('next').NextConfig} */\nconst nextConfig = {};\n\nexport default nextConfig;\n",
    },
    Template {
        path: "app/layout.tsx",
        body: "export const metadata = {\n  title: 'App',\n};\n\nexport default function RootLayout({\n  children,\n}: {\n  children: React.ReactNode;\n}) {\n  return (\n    <html lang=\"ja\">\n      <body>{children}</body>\n    </html>\n  );\n}\n",
    },
    Template {
        path: "app/page.tsx",
        body: "export default function Page() {\n  return <main>Ready.</main>;\n}\n",
    },
];

/// Why a template was not written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Skipped {
    /// Something of that name is already there. Its contents are not read
    /// and never compared: a project's own file is the project's.
    AlreadyPresent,
    /// The name is taken by something that is not a regular file — a
    /// directory, a symlink, a device. Writing through it would land
    /// somewhere nobody asked for.
    NotAPlainFile,
}

/// What would be written and what would be left alone. Decided by looking,
/// so the caller can show it before anything is written.
pub fn plan(
    project_root: &Path,
    templates: &[Template],
) -> (Vec<Template>, Vec<(&'static str, Skipped)>) {
    let mut write = Vec::new();
    let mut skip = Vec::new();
    for template in templates {
        // `symlink_metadata` does not follow, so a symlink reports as a
        // symlink and never as the file it points at.
        match std::fs::symlink_metadata(project_root.join(template.path)) {
            Ok(held) if held.is_file() => {
                skip.push((template.path, Skipped::AlreadyPresent));
            }
            Ok(_) => skip.push((template.path, Skipped::NotAPlainFile)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => write.push(*template),
            // Unreadable is not the same as absent, and is never written over.
            Err(_) => skip.push((template.path, Skipped::NotAPlainFile)),
        }
    }
    (write, skip)
}

/// Writes the templates the plan chose. Creates the directories they sit in,
/// and refuses a file that appeared between the plan and the write rather
/// than replacing it.
pub fn write(project_root: &Path, templates: &[Template]) -> std::io::Result<Vec<&'static str>> {
    let mut written = Vec::new();
    for template in templates {
        let path = project_root.join(template.path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut file) => {
                use std::io::Write;
                file.write_all(template.body.as_bytes())?;
                file.sync_all()?;
                written.push(template.path);
            }
            // `create_new` is what makes this safe against a file that
            // appeared after the plan was made: it fails rather than
            // truncating.
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    Ok(written)
}

#[cfg(test)]
mod tests;
