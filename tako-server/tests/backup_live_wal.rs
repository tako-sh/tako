#![cfg(target_os = "linux")]

use landlock::{
    ABI, Access, AccessFs, CompatLevel, Compatible, PathBeneath, Ruleset, RulesetAttr,
    RulesetCreated, RulesetCreatedAttr,
};
use std::fs::{self, File};
use std::io;
use std::os::fd::OwnedFd;
use std::path::Path;
use std::process::{Command, Output, Stdio};

fn capture(rules: RulesetCreated, source: &Path, dest: &Path) -> Output {
    let fd: Option<OwnedFd> = rules.into();
    Command::new(env!("CARGO_BIN_EXE_tako-server"))
        .current_dir(source)
        .args(["--snapshot-live-wal", "./db"])
        .arg(dest)
        .stdin(fd.unwrap())
        .output()
        .unwrap()
}

#[test]
fn live_wal_worker_requires_a_ruleset_before_opening_files() {
    use rustix::fs::inotify;

    let tmp = tempfile::tempdir().unwrap();
    let source = tmp.path().join("source");
    rusqlite::Connection::open(&source)
        .unwrap()
        .execute("CREATE TABLE items(value)", [])
        .unwrap();
    let watch =
        inotify::init(inotify::CreateFlags::NONBLOCK | inotify::CreateFlags::CLOEXEC).unwrap();
    inotify::add_watch(&watch, &source, inotify::WatchFlags::OPEN).unwrap();
    let dest = tmp.path().join("snapshot");
    let result = Command::new(env!("CARGO_BIN_EXE_tako-server"))
        .arg("--snapshot-live-wal")
        .arg(source)
        .arg(&dest)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert_eq!(
        rustix::io::read(&watch, &mut [0u8; 128][..]),
        Err(rustix::io::Errno::AGAIN),
        "worker opened the source before enforcing its ruleset"
    );
    assert!(!dest.exists());
}

#[test]
fn live_wal_worker_rejects_hardlinks_substituted_after_pinning() {
    let tmp = tempfile::tempdir().unwrap();
    let app = tmp.path().join("app");
    let private = tmp.path().join("private");
    fs::create_dir(&app).unwrap();
    fs::create_dir(&private).unwrap();
    let db = rusqlite::Connection::open(app.join("db")).unwrap();
    db.execute_batch(
        "PRAGMA journal_mode=WAL; CREATE TABLE items(value); PRAGMA wal_checkpoint(TRUNCATE);
         INSERT INTO items VALUES (42);",
    )
    .unwrap();
    let names = ["db", "db-wal", "db-shm"];
    let pinned = names.map(|name| File::open(app.join(name)).unwrap());
    for (name, mut original) in names.into_iter().zip(&pinned) {
        let Ok(mut rules) = Ruleset::default()
            .set_compatibility(CompatLevel::HardRequirement)
            .handle_access(AccessFs::from_all(ABI::V3))
            .and_then(Ruleset::create)
        else {
            eprintln!("skipping live WAL worker test: Landlock ABI 3 unavailable");
            return;
        };
        for fd in &pinned {
            rules = rules
                .add_rule(PathBeneath::new(fd, AccessFs::ReadFile))
                .unwrap();
        }
        let scratch = AccessFs::ReadFile
            | AccessFs::WriteFile
            | AccessFs::ReadDir
            | AccessFs::MakeReg
            | AccessFs::RemoveFile
            | AccessFs::Truncate;
        rules = rules
            .add_rule(PathBeneath::new(File::open(&private).unwrap(), scratch))
            .unwrap();

        let outside = tmp.path().join(format!("outside-{name}"));
        let original_name = app.join(name);
        let saved = app.join("original");
        let replacement = File::create_new(&outside).unwrap();
        // Use identical valid SQLite bytes so malformed input cannot explain a refusal.
        io::copy(&mut original, &mut &replacement).unwrap();
        fs::rename(&original_name, &saved).unwrap();
        fs::hard_link(&outside, &original_name).unwrap();

        let dest = private.join(format!("denied-{name}"));
        let denied = capture(rules.try_clone().unwrap(), &app, &dest);
        assert!(
            !denied.status.success(),
            "worker accepted substituted {name}"
        );
        assert!(
            match fs::metadata(dest) {
                Ok(meta) => meta.len() == 0,
                Err(e) => e.kind() == io::ErrorKind::NotFound,
            },
            "captured bytes from {name}"
        );

        // Granting access to just the substituted inode must make the same capture succeed.
        let allowed = rules
            .add_rule(PathBeneath::new(&replacement, AccessFs::ReadFile))
            .unwrap();
        let dest = private.join(format!("allowed-{name}"));
        let control = capture(allowed, &app, &dest);
        fs::remove_file(&original_name).unwrap();
        fs::rename(&saved, &original_name).unwrap();
        if control.status.code() == Some(77) {
            assert_eq!(denied.status.code(), Some(77));
            eprintln!("skipping live WAL worker test: kernel refused to activate the sandbox");
            break;
        }
        assert!(
            control.status.success(),
            "{name}: {}",
            String::from_utf8_lossy(&control.stderr)
        );
        let restored = rusqlite::Connection::open(dest).unwrap();
        let value: i64 = restored
            .query_row("SELECT value FROM items", [], |row| row.get(0))
            .unwrap();
        assert_eq!(value, 42, "{name}");
    }
    // Closing any descriptor for a live inode releases this process's POSIX locks.
    drop(db);
    drop(pinned);
}
