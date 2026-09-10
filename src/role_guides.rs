//! Compile role procedures into every binary, then expose only their index in
//! the turn. File reads are provider-native; no Skill installation is required.
//! If the cache cannot be prepared, the complete procedures travel inline.

use std::{
    collections::hash_map::DefaultHasher,
    fs,
    hash::{Hash, Hasher},
    io::{self, Write},
    path::{Path, PathBuf},
};

pub struct Guide {
    pub name: &'static str,
    pub body: &'static str,
}

pub fn cache_root() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .unwrap_or_else(std::env::temp_dir)
        .join("DevezVibe")
        .join("role-guides")
}

pub fn render(core: &str, role: &str, guides: &[Guide], root: &Path) -> String {
    if guides.is_empty() {
        return core.trim().to_owned();
    }
    let directory = provision(root, role, guides);
    let delivery = match &directory {
        Ok(directory) => format!(
            "<devez-role-guides>\nDirectory: {}\n\
             Mandatory procedure gate: before the first repository read, command, edit or judgment for a stage, \
             read its entire indexed guide with an available file-reading tool. Progress/task-list tools may precede it. \
             The core is a guardrail and index, not a substitute for procedures; even one-line fixes and self-review require their guides. \
             Read only applicable guides, not the whole directory. Reuse a guide only while its full content \
             from this directory remains in context; after resume/compression read the needed guide again. \
             If reading fails, mark that stage incomplete using the role's final-report format; \
             never act, guess, skip a gate or search other directories for a substitute. \
             Supplied-record reporting needs no guide reads.\n</devez-role-guides>",
            serde_json::to_string(&directory.to_string_lossy().replace('\\', "/")).unwrap()
        ),
        Err(error) => {
            let mut inline = format!(
                "<devez-role-guides>\nDelivery: inline; reference preparation failed ({:?}). \
                 All indexed guides are ALREADY LOADED in full below. This satisfies the procedure-read gate. \
                 Apply only the stages needed; do not search for or open procedure files.\n",
                error.kind()
            );
            for guide in guides {
                inline.push_str(&format!(
                    "\n<guide name=\"{}\">\n{}\n</guide>\n",
                    guide.name,
                    guide.body.trim()
                ));
            }
            inline.push_str("</devez-role-guides>");
            inline
        }
    };
    let mut rendered = core.trim().replace("{{ROLE_GUIDES}}", &delivery);
    if let Ok(directory) = directory {
        // Give each entry an actionable absolute path. A separate root plus
        // bare filenames can be mistaken for optional background references.
        for guide in guides {
            let path = directory
                .join(guide.name)
                .to_string_lossy()
                .replace('\\', "/");
            rendered = rendered.replace(
                &format!("`{}`", guide.name),
                &format!("[{}](<{}>)", guide.name, path),
            );
        }
    }
    rendered
}

fn provision(root: &Path, role: &str, guides: &[Guide]) -> io::Result<PathBuf> {
    if !root.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "guide cache must be absolute",
        ));
    }
    // This is a cache key, not an integrity/security hash. Exact bytes are
    // checked on every turn. Changed procedures get a new directory so a
    // running old binary keeps its own references through an update.
    let mut key = DefaultHasher::new();
    role.hash(&mut key);
    for guide in guides {
        guide.name.hash(&mut key);
        guide.body.hash(&mut key);
    }
    let directory = root.join(format!("{role}-{:016x}", key.finish()));
    if directory.to_str().is_none() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "guide path is not Unicode",
        ));
    }
    fs::create_dir_all(&directory)?;
    for guide in guides {
        let path = directory.join(guide.name);
        match fs::read(&path) {
            Ok(bytes) => verify(&bytes, guide.body)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                match fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&path)
                {
                    Ok(mut file) => {
                        let result = file
                            .write_all(guide.body.as_bytes())
                            .and_then(|_| file.sync_all());
                        drop(file);
                        if let Err(error) = result {
                            // Only a file created by this call is removed. Existing
                            // user-edited/corrupt entries are never overwritten.
                            let _ = fs::remove_file(&path);
                            return Err(error);
                        }
                    }
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                    Err(error) => return Err(error),
                }
                // A concurrent writer may still be filling the file. In that
                // case return the inline form, never a partial guide reference.
                verify(&fs::read(&path)?, guide.body)?;
            }
            Err(error) => return Err(error),
        }
    }
    Ok(directory)
}

fn verify(bytes: &[u8], expected: &str) -> io::Result<()> {
    if bytes == expected.as_bytes() {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "guide contents differ from this binary",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    pub fn test_root() -> PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        std::env::temp_dir().join(format!(
            "devez-role-guides-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ))
    }

    const CORE: &str = "Always preserve authority.\n{{ROLE_GUIDES}}\nRead review.md before review.";
    const GUIDES: &[Guide] = &[
        Guide {
            name: "review.md",
            body: "Reject mismatched evidence.\n",
        },
        Guide {
            name: "finish.md",
            body: "Check the final artifact.\n",
        },
    ];

    #[test]
    fn repeated_reads_preserve_bytes_and_missing_files_are_recreated() {
        let root = test_root();
        let directory = provision(&root, "reviewer", GUIDES).unwrap();
        let path = directory.join("review.md");
        let modified = fs::metadata(&path).unwrap().modified().unwrap();
        assert_eq!(provision(&root, "reviewer", GUIDES).unwrap(), directory);
        assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), modified);
        fs::remove_file(&path).unwrap();
        assert_eq!(provision(&root, "reviewer", GUIDES).unwrap(), directory);
        assert_eq!(fs::read_to_string(&path).unwrap(), GUIDES[0].body);
        let block = render(CORE, "reviewer", GUIDES, &root);
        assert!(block.contains("Directory: "));
        assert!(!block.contains(GUIDES[0].body));
        assert!(!block.contains("{{ROLE_GUIDES}}"));
    }

    #[test]
    fn changed_procedures_keep_the_old_binarys_files() {
        let root = test_root();
        let old = provision(&root, "reviewer", GUIDES).unwrap();
        let updated = [Guide {
            name: "review.md",
            body: "New review contract.\n",
        }];
        let new = provision(&root, "reviewer", &updated).unwrap();
        assert_ne!(old, new);
        assert_eq!(
            fs::read_to_string(old.join("review.md")).unwrap(),
            GUIDES[0].body
        );
        assert_eq!(
            fs::read_to_string(new.join("review.md")).unwrap(),
            updated[0].body
        );
    }

    #[test]
    fn corrupt_entries_stay_untouched_and_all_guides_fall_back_inline() {
        let root = test_root();
        let directory = provision(&root, "reviewer", GUIDES).unwrap();
        let path = directory.join("review.md");
        fs::write(&path, "user edit").unwrap();
        let block = render(CORE, "reviewer", GUIDES, &root);
        assert!(block.contains("Delivery: inline"));
        assert!(!block.contains("Directory: "));
        for guide in GUIDES {
            assert!(block.contains(guide.body.trim()));
        }
        assert_eq!(fs::read_to_string(path).unwrap(), "user edit");
    }

    #[test]
    fn unavailable_or_relative_cache_does_not_drop_procedures() {
        let root = test_root();
        fs::write(&root, "occupied").unwrap();
        for path in [root.as_path(), Path::new("relative-cache")] {
            let block = render(CORE, "reviewer", GUIDES, path);
            assert!(block.contains("Delivery: inline"));
            for guide in GUIDES {
                assert!(block.contains(guide.body.trim()));
            }
        }
        assert_eq!(fs::read_to_string(root).unwrap(), "occupied");
    }

    #[test]
    fn concurrent_provision_never_exposes_partial_content() {
        let root = test_root();
        let outputs = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..8)
                .map(|_| scope.spawn(|| render(CORE, "reviewer", GUIDES, &root)))
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .collect::<Vec<_>>()
        });
        for output in outputs {
            if output.contains("Directory: ") {
                let directory = provision(&root, "reviewer", GUIDES).unwrap();
                for guide in GUIDES {
                    assert_eq!(
                        fs::read_to_string(directory.join(guide.name)).unwrap(),
                        guide.body
                    );
                }
            } else {
                assert!(output.contains("Delivery: inline"));
                for guide in GUIDES {
                    assert!(output.contains(guide.body.trim()));
                }
            }
        }
    }

    #[cfg(windows)]
    #[test]
    fn an_unrepresentable_path_uses_inline_instead_of_a_lossy_reference() {
        use std::os::windows::ffi::OsStringExt;
        let root = test_root().join(std::ffi::OsString::from_wide(&[0xd800]));
        let block = render(CORE, "reviewer", GUIDES, &root);
        assert!(block.contains("Delivery: inline"));
        assert!(!block.contains("Directory: "));
        assert!(!root.exists());
        for guide in GUIDES {
            assert!(block.contains(guide.body.trim()));
        }
    }
}
