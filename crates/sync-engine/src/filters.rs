//! Which file names are never synced.
//!
//! Lives apart from the engine because the database needs the same answer when
//! clearing out items that a newly added pattern now excludes, and the FUSE
//! layer needs it on the upload path.

/// Whether a file name matches any exclusion pattern.
///
/// In on-demand mode the local filesystem watcher does not run, so this is the
/// only thing standing between an editor's lock file and OneDrive.
pub fn is_excluded_name(name: &str, patterns: &[String]) -> bool {
    patterns.iter().any(|p| name_matches_pattern(name, p))
}

/// Simple glob matching: only supports leading/trailing wildcards.
/// Case-insensitive, since OneDrive itself is case-insensitive and Windows
/// artifacts vary in casing (e.g. `Thumbs.db` vs `thumbs.db`).
fn name_matches_pattern(name: &str, pattern: &str) -> bool {
    let name = name.to_lowercase();
    let pattern = pattern.to_lowercase();
    if let Some(inner) = pattern.strip_prefix('*').and_then(|p| p.strip_suffix('*')) {
        name.contains(inner)
    } else if let Some(suffix) = pattern.strip_prefix('*') {
        name.ends_with(suffix)
    } else if let Some(prefix) = pattern.strip_suffix('*') {
        name.starts_with(prefix)
    } else {
        name == pattern
    }
}

#[cfg(test)]
mod exclusion_tests {
    use super::is_excluded_name;

    fn defaults() -> Vec<String> {
        crate::config::Config::default_excluded_patterns()
    }

    #[test]
    fn default_patterns_catch_the_artefacts_they_name() {
        let patterns = defaults();
        for name in [
            "draft.tmp",
            "~$report.docx",
            ".~lock.budget.ods#",
            "desktop.ini",
            "Thumbs.db", // casing varies; OneDrive itself is case-insensitive
        ] {
            assert!(
                is_excluded_name(name, &patterns),
                "{name} should be excluded by the defaults"
            );
        }
    }

    #[test]
    fn ordinary_files_are_not_excluded() {
        let patterns = defaults();
        for name in ["report.docx", "notes.txt", "photo.jpg", "temporary-plan.md"] {
            assert!(
                !is_excluded_name(name, &patterns),
                "{name} must not be excluded"
            );
        }
    }

    #[test]
    fn no_patterns_excludes_nothing() {
        assert!(!is_excluded_name("anything.tmp", &[]));
    }
}

#[cfg(test)]
mod pattern_tests {
    use super::name_matches_pattern;

    #[test]
    fn exact_match() {
        assert!(name_matches_pattern("desktop.ini", "desktop.ini"));
        assert!(!name_matches_pattern("desktop.ini.bak", "desktop.ini"));
    }

    #[test]
    fn suffix_wildcard() {
        assert!(name_matches_pattern("notes.tmp", "*.tmp"));
        assert!(!name_matches_pattern("notes.txt", "*.tmp"));
    }

    #[test]
    fn prefix_wildcard() {
        assert!(name_matches_pattern("~$report.docx", "~$*"));
        assert!(name_matches_pattern(".~lock.file.odt", ".~lock.*"));
        assert!(!name_matches_pattern("report.docx", "~$*"));
    }

    #[test]
    fn contains_wildcard() {
        assert!(name_matches_pattern("a.partial.download", "*partial*"));
        assert!(!name_matches_pattern("complete.download", "*partial*"));
    }

    #[test]
    fn case_insensitive() {
        assert!(name_matches_pattern("Thumbs.db", "thumbs.db"));
        assert!(name_matches_pattern("NOTES.TMP", "*.tmp"));
    }

    #[test]
    fn bare_star_matches_everything() {
        assert!(name_matches_pattern("anything", "*"));
    }
}
