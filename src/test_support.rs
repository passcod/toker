//! Temporary directories shared by unit tests.

/// A [`tempfile::TempDir`] that dereferences to its path for terse fixtures.
pub(crate) struct TestDir(tempfile::TempDir);

impl std::ops::Deref for TestDir {
    type Target = std::path::Path;

    fn deref(&self) -> &Self::Target {
        self.0.path()
    }
}

impl AsRef<std::path::Path> for TestDir {
    fn as_ref(&self) -> &std::path::Path {
        self.0.path()
    }
}

/// A fresh directory under `/tmp/opencode`, removed when its guard drops.
pub(crate) fn tempdir(prefix: &str) -> TestDir {
    let root = std::path::Path::new("/tmp/opencode");
    std::fs::create_dir_all(root).expect("create test scratch root");
    TestDir(
        tempfile::Builder::new()
            .prefix(prefix)
            .tempdir_in(root)
            .expect("create temporary test directory"),
    )
}

#[cfg(test)]
mod tests {
    #[test]
    fn dropping_the_guard_removes_the_tree() {
        let dir = super::tempdir("cleanup-proof-");
        let path = dir.to_path_buf();
        std::fs::write(path.join("fixture"), b"fixture").expect("write fixture");
        drop(dir);
        assert!(!path.exists(), "tempfile removed the tree");
    }
}
