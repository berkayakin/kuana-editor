//! Which of the registry's extensions covers a file Kuana has no language
//! for, so that opening one can offer to install it. A table, as in Zed: the
//! registry's index doesn't say which files an extension's languages take.
//! An extension missing here is only never offered; it installs from
//! Settings → Extensions like any other.

use std::{
    collections::HashSet,
    path::Path,
    sync::{LazyLock, Mutex},
};

/// An extension's id in the registry, the file suffixes it takes (lowercase,
/// without the dot) and whole file names. A suffix or name is listed once:
/// where two extensions take it (Oracle's also takes `.sql`), the more
/// general one has it.
const KNOWN: &[(&str, &[&str], &[&str])] = &[
    ("astro", &["astro"], &[]),
    ("c-cpp", &["c", "h", "cc", "cpp", "cxx", "hh", "hpp", "hxx", "ipp", "inl", "tpp", "ino"], &[]),
    ("clojure", &["clj", "cljs", "cljc", "edn"], &[]),
    ("csharp", &["cs", "csproj", "slnx"], &[]),
    ("csv", &["csv"], &[]),
    ("dart", &["dart"], &[]),
    ("dockerfile", &["dockerfile"], &["Dockerfile", "Containerfile"]),
    ("elixir", &["ex", "exs", "heex"], &[]),
    ("env", &["env"], &[".env"]),
    ("erlang", &["erl", "hrl"], &[]),
    ("fish", &["fish"], &[]),
    ("fsharp", &["fs", "fsx", "fsi"], &[]),
    ("gdscript", &["gd"], &[]),
    ("git-firefly", &[], &[".gitignore", ".gitattributes", ".gitmodules"]),
    ("graphql", &["graphql", "gql"], &[]),
    ("haskell", &["hs"], &[]),
    ("ini", &["ini", "cfg"], &[]),
    ("java", &["java"], &[]),
    ("json5", &["json5"], &[]),
    ("julia", &["jl"], &[]),
    ("just", &["just"], &["justfile", "Justfile", ".justfile"]),
    ("kotlin", &["kt", "kts"], &[]),
    ("log", &["log"], &[]),
    ("lua", &["lua"], &[]),
    ("luau", &["luau"], &[]),
    ("lyth", &["lyth"], &[]),
    ("make", &["mk", "mak"], &["Makefile", "makefile", "GNUmakefile"]),
    ("neocmake", &["cmake"], &["CMakeLists.txt"]),
    ("nix", &["nix"], &[]),
    ("nu", &["nu"], &[]),
    ("ocaml", &["ml", "mli"], &[]),
    ("odin", &["odin"], &[]),
    ("oracle", &["pls", "plb", "pks", "pkb", "pkg", "pck", "spc", "bdy", "prc", "fnc", "fct", "trg", "tps", "tpb", "vw"], &[]),
    ("oxie", &["oxi"], &[]),
    ("perl", &["pl", "pm"], &[]),
    ("powershell", &["ps1", "psm1", "psd1"], &[]),
    ("prisma", &["prisma"], &[]),
    ("proto", &["proto"], &[]),
    ("python-requirements", &[], &["requirements.txt"]),
    ("r", &["r"], &[]),
    ("ruby", &["rb", "rake", "gemspec"], &["Gemfile", "Rakefile"]),
    ("scala", &["scala", "sbt"], &[]),
    ("scss", &["scss"], &[]),
    ("solidity", &["sol"], &[]),
    ("sql", &["sql"], &[]),
    ("svelte", &["svelte"], &[]),
    ("swift", &["swift"], &[]),
    ("terraform", &["tf", "tfvars", "hcl"], &[]),
    ("typst", &["typ"], &[]),
    ("vue", &["vue"], &[]),
    ("xml", &["xml", "xsd", "xsl", "xslt"], &[]),
    ("zig", &["zig", "zon"], &[]),
];

/// The registry's extension for a file, by its name: a whole name first
/// (`Makefile`), then what follows its last dot, in any case (`A.SQL`).
pub fn for_path(path: &Path) -> Option<&'static str> {
    let name = path.file_name()?.to_str()?;
    if let Some((id, ..)) = KNOWN.iter().find(|(_, _, names)| names.contains(&name)) {
        return Some(id);
    }
    let (stem, suffix) = name.rsplit_once('.')?;
    if stem.is_empty() {
        return None;
    }
    let suffix = suffix.to_ascii_lowercase();
    KNOWN.iter().find(|(_, suffixes, _)| suffixes.contains(&suffix.as_str())).map(|(id, ..)| *id)
}

/// Extensions offered, or being installed from an offer, since Kuana
/// started: in any window, so two of them don't offer the same one.
static OFFERED: LazyLock<Mutex<HashSet<&'static str>>> = LazyLock::new(Mutex::default);
static INSTALLING: LazyLock<Mutex<HashSet<&'static str>>> = LazyLock::new(Mutex::default);

/// Notes that `id` is being offered; false if it was already, this run.
pub fn offer_once(id: &'static str) -> bool {
    OFFERED.lock().unwrap().insert(id)
}

/// The offer came to nothing (the registry couldn't be read): a later file may try again.
pub fn forget_offer(id: &'static str) {
    OFFERED.lock().unwrap().remove(id);
}

/// Notes that `id` is being installed; false if it already is.
pub fn start_install(id: &'static str) -> bool {
    INSTALLING.lock().unwrap().insert(id)
}

pub fn end_install(id: &'static str) {
    INSTALLING.lock().unwrap().remove(id);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_an_extension_by_suffix_or_whole_name() {
        assert_eq!(for_path(Path::new("/p/query.sql")), Some("sql"));
        assert_eq!(for_path(Path::new("REPORT.SQL")), Some("sql"));
        assert_eq!(for_path(Path::new("/p/billing.pkb")), Some("oracle"));
        assert_eq!(for_path(Path::new("src/main.cpp")), Some("c-cpp"));
        assert_eq!(for_path(Path::new("/p/Makefile")), Some("make"));
        assert_eq!(for_path(Path::new("/p/CMakeLists.txt")), Some("neocmake"));
        assert_eq!(for_path(Path::new("/p/.env")), Some("env"));
        assert_eq!(for_path(Path::new("/p/prod.env")), Some("env"));
        // Names match as written; nothing for other files, or ones without a suffix.
        assert_eq!(for_path(Path::new("/p/makefile.txt")), None);
        assert_eq!(for_path(Path::new("/p/notes.txt")), None);
        assert_eq!(for_path(Path::new("/p/sql")), None);
        assert_eq!(for_path(Path::new("/p/.sql")), None);
    }

    /// Each id, suffix and name is there once, suffixes are lowercase, and
    /// none is a file a built-in language already takes.
    #[test]
    fn the_table_has_no_repeats_and_nothing_built_in() {
        let mut ids = HashSet::new();
        let mut taken = HashSet::new();
        for (id, suffixes, names) in KNOWN {
            assert!(ids.insert(*id), "{id} is listed twice");
            assert!(!suffixes.is_empty() || !names.is_empty(), "{id} takes no file");
            for suffix in *suffixes {
                assert_eq!(*suffix, suffix.to_ascii_lowercase(), "{id}: suffixes are lowercase");
                assert!(taken.insert(format!("*.{suffix}")), "{id}: .{suffix} is listed twice");
                let file = format!("file.{suffix}");
                assert!(crate::languages::for_path_with_len(Path::new(&file)).is_none(), "{id}: {file} is built in");
                assert_eq!(for_path(Path::new(&file)), Some(*id));
            }
            for name in *names {
                assert!(taken.insert(name.to_string()), "{id}: {name} is listed twice");
                assert!(crate::languages::for_path_with_len(Path::new(name)).is_none(), "{id}: {name} is built in");
                assert_eq!(for_path(Path::new(name)), Some(*id));
            }
        }
    }

    #[test]
    fn an_offer_is_made_once_until_forgotten() {
        assert!(offer_once("test-only-extension"));
        assert!(!offer_once("test-only-extension"));
        forget_offer("test-only-extension");
        assert!(offer_once("test-only-extension"));
        assert!(start_install("test-only-extension") && !start_install("test-only-extension"));
        end_install("test-only-extension");
        assert!(start_install("test-only-extension"));
    }
}
