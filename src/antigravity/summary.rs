//! SQLite summary snapshots on Windows. Delegate locking and WAL interpretation to SQLite.

use std::ffi::{c_char, c_int, c_void, CStr};
use std::fmt::Write as _;
use std::path::Path;
use std::sync::OnceLock;
use std::time::Duration;

use rusqlite::{config::DbConfig, ffi, Connection, OpenFlags, OptionalExtension};

use crate::probe::{self, Probed, RealEntryKind};

const VFS_NAME: &CStr = c"svault-antigravity-readonly";

pub(super) fn read(root: &Path, id: &str) -> Result<Option<String>, String> {
    let path = root.join(super::SUMMARY_DB);
    let main_present = regular_file(&path)?;
    let wal = path.with_extension("db-wal");
    let shm = path.with_extension("db-shm");
    let journal = path.with_extension("db-journal");
    let wal_present = regular_file(&wal)?;
    let shm_present = regular_file(&shm)?;
    let journal_present = regular_file(&journal)?;
    if !main_present {
        return if wal_present || shm_present || journal_present {
            Err("Antigravity CLI summary is missing while a sidecar exists; retry".into())
        } else {
            Ok(None)
        };
    }

    match open(&path, "readonly_shm=1").and_then(|conn| query(&conn, id)) {
        Ok(value) => Ok(value),
        Err(error) => {
            // A clean WAL shutdown removes the auxiliary files but leaves WAL mode in the header.
            // Only use immutable when Windows has excluded every writable/deletable main-file
            // handle and no WAL or rollback journal exists under that protection.
            if wal_present || journal_present {
                return Err(snapshot_error(error));
            }
            let Ok(_pin) = probe::pin_windows_file_against_writes(&path) else {
                return Err(snapshot_error(error));
            };
            if !regular_file(&wal)? && !regular_file(&journal)? {
                return open(&path, "immutable=1")
                    .and_then(|conn| query(&conn, id))
                    .map_err(snapshot_error);
            }
            Err(snapshot_error(error))
        }
    }
}

fn regular_file(path: &Path) -> Result<bool, String> {
    match probe::lstat_entry(path, path.parent()) {
        Probed::Found(RealEntryKind::RegularFile(_)) => Ok(true),
        Probed::Absent => Ok(false),
        Probed::Found(_) => Err("Antigravity CLI summary or sidecar is not a regular file".into()),
        Probed::Unknown(error) => Err(error.to_string()),
    }
}

fn snapshot_error(error: rusqlite::Error) -> String {
    format!("Antigravity CLI summary snapshot unavailable; retry: {error}")
}

fn open(path: &Path, options: &str) -> rusqlite::Result<Connection> {
    register_vfs().map_err(|code| rusqlite::Error::SqliteFailure(ffi::Error::new(code), None))?;
    let path = path
        .to_str()
        .ok_or(rusqlite::Error::InvalidPath(path.to_owned()))?;
    let mut uri = String::from("file:/");
    for byte in path.bytes() {
        if byte == b'\\' {
            uri.push('/');
        } else if byte.is_ascii_alphanumeric()
            || matches!(byte, b'-' | b'_' | b'.' | b'~' | b'/' | b':')
        {
            uri.push(char::from(byte));
        } else {
            let _ = write!(uri, "%{byte:02X}");
        }
    }
    let _ = write!(uri, "?mode=ro&{options}");
    let conn = Connection::open_with_flags_and_vfs(
        uri,
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_URI
            | OpenFlags::SQLITE_OPEN_PRIVATE_CACHE,
        VFS_NAME.to_str().expect("ASCII VFS name"),
    )?;
    conn.busy_timeout(Duration::ZERO)?;
    conn.set_db_config(DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE, true)?;
    conn.pragma_update(None, "trusted_schema", false)?;
    Ok(conn)
}

fn query(conn: &Connection, id: &str) -> rusqlite::Result<Option<String>> {
    // A single SELECT runs in a SQLite read transaction and sees one committed snapshot.
    conn.query_row(
        "SELECT workspace_uris FROM conversation_summaries WHERE conversation_id = ?1",
        [id],
        |row| row.get::<_, Option<String>>(0),
    )
    .optional()
    .map(Option::flatten)
}

// SQLite's READ_ONLY flag applies to the main database, but the pager still requests
// READWRITE|CREATE for WAL files. Preserve the Win32 lock implementation and narrow
// file opens for this VFS only. Keep the original methods for callbacks that we wrap.
#[repr(C)]
struct ReadVfs {
    vfs: ffi::sqlite3_vfs,
    native: *const ffi::sqlite3_vfs,
}

fn register_vfs() -> Result<(), c_int> {
    static REGISTERED: OnceLock<Result<(), c_int>> = OnceLock::new();
    *REGISTERED.get_or_init(|| {
        // SAFETY: The bundled Win32 VFS and its app data are static. On success SQLite
        // retains a pointer to our copy, so we allocate it once for the process lifetime.
        unsafe {
            let native = ffi::sqlite3_vfs_find(c"win32".as_ptr());
            if native.is_null() {
                return Err(ffi::SQLITE_CANTOPEN);
            }
            let mut vfs = Box::new(ReadVfs {
                vfs: *native,
                native,
            });
            vfs.vfs.pNext = std::ptr::null_mut();
            vfs.vfs.zName = VFS_NAME.as_ptr();
            vfs.vfs.xOpen = Some(open_read_only);
            vfs.vfs.xDelete = Some(reject_delete);
            let vfs = Box::into_raw(vfs);
            let rc = ffi::sqlite3_vfs_register(vfs.cast(), 0);
            if rc != ffi::SQLITE_OK {
                drop(Box::from_raw(vfs));
                return Err(rc);
            }
            Ok(())
        }
    })
}

unsafe extern "C" fn open_read_only(
    vfs: *mut ffi::sqlite3_vfs,
    name: *const c_char,
    file: *mut ffi::sqlite3_file,
    flags: c_int,
    out_flags: *mut c_int,
) -> c_int {
    let source_flags =
        ffi::SQLITE_OPEN_MAIN_DB | ffi::SQLITE_OPEN_MAIN_JOURNAL | ffi::SQLITE_OPEN_WAL;
    // SAFETY: SQLite supplies a filename (NUL-terminated when non-null), storage sized by
    // the native VFS, and the address of the first field of the registered ReadVfs.
    unsafe {
        (*file).pMethods = std::ptr::null();
        if name.is_null() || flags & source_flags == 0 {
            return ffi::SQLITE_CANTOPEN;
        }
        let Ok(path) = CStr::from_ptr(name).to_str() else {
            return ffi::SQLITE_CANTOPEN;
        };
        if !matches!(regular_file(Path::new(path)), Ok(true)) {
            return ffi::SQLITE_CANTOPEN;
        }
        let native = (*vfs.cast::<ReadVfs>()).native;
        let native_open = (*native).xOpen.expect("Win32 VFS has xOpen");
        let read_only = (flags | ffi::SQLITE_OPEN_READONLY)
            & !(ffi::SQLITE_OPEN_READWRITE
                | ffi::SQLITE_OPEN_CREATE
                | ffi::SQLITE_OPEN_DELETEONCLOSE);
        let rc = native_open(vfs, name, file, read_only, out_flags);
        if rc == ffi::SQLITE_OK && flags & ffi::SQLITE_OPEN_MAIN_DB != 0 {
            install_methods(file);
        }
        rc
    }
}

unsafe extern "C" fn reject_delete(_: *mut ffi::sqlite3_vfs, _: *const c_char, _: c_int) -> c_int {
    ffi::SQLITE_READONLY
}

#[repr(C)]
struct ReadMethods {
    methods: ffi::sqlite3_io_methods,
    native: *const ffi::sqlite3_io_methods,
}

unsafe fn install_methods(file: *mut ffi::sqlite3_file) {
    // SAFETY: The native xOpen succeeded. Its method table is static for this file.
    unsafe {
        let native = (*file).pMethods;
        let mut table = Box::new(ReadMethods {
            methods: *native,
            native,
        });
        table.methods.xClose = Some(close_read_only);
        table.methods.xShmMap = Some(map_shm_read_only);
        table.methods.xShmUnmap = Some(unmap_shm_read_only);
        (*file).pMethods = Box::into_raw(table).cast();
    }
}

unsafe extern "C" fn close_read_only(file: *mut ffi::sqlite3_file) -> c_int {
    // SAFETY: install_methods placed an allocated ReadMethods at pMethods. SQLite calls
    // xClose once, and the native close may reset pMethods, so restore it first.
    unsafe {
        let table = Box::from_raw((*file).pMethods.cast::<ReadMethods>().cast_mut());
        (*file).pMethods = table.native;
        ((*table.native).xClose.expect("Win32 file has xClose"))(file)
    }
}

unsafe extern "C" fn map_shm_read_only(
    file: *mut ffi::sqlite3_file,
    page: c_int,
    page_size: c_int,
    _extend: c_int,
    region: *mut *mut c_void,
) -> c_int {
    // SAFETY: The method is called only on a file with our table and SQLite's out pointer.
    // A same-process writer can already own a writable Win32 mapping even with the URI
    // option set. Never extend it or let SQLite treat it as writable (e.g. set read marks).
    unsafe {
        let native = (*(*file).pMethods.cast::<ReadMethods>()).native;
        let map = (*native).xShmMap.expect("Win32 VFS has xShmMap");
        match map(file, page, page_size, 0, region) {
            ffi::SQLITE_OK | ffi::SQLITE_READONLY if (*region).is_null() => {
                ffi::SQLITE_IOERR_SHMMAP
            }
            ffi::SQLITE_OK => ffi::SQLITE_READONLY,
            rc => rc,
        }
    }
}

unsafe extern "C" fn unmap_shm_read_only(file: *mut ffi::sqlite3_file, _delete: c_int) -> c_int {
    // SAFETY: The installed table retains the original VFS methods for this handle.
    unsafe {
        let native = (*(*file).pMethods.cast::<ReadMethods>()).native;
        ((*native).xShmUnmap.expect("Win32 VFS has xShmUnmap"))(file, 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[allow(clippy::disallowed_methods)]
    fn read_snapshot_survives_an_overlapping_commit_and_checkpoint() {
        use std::sync::atomic::{AtomicU64, Ordering};

        static NEXT_DIR: AtomicU64 = AtomicU64::new(0);
        let root = loop {
            let root = std::env::temp_dir().join(format!(
                "svault-summary-snapshot-{}-{}",
                std::process::id(),
                NEXT_DIR.fetch_add(1, Ordering::Relaxed)
            ));
            match std::fs::create_dir(&root) {
                Ok(()) => break root,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("create fixture directory: {error}"),
            }
        };
        let path = root.join(super::super::SUMMARY_DB);
        let writer = Connection::open(&path).unwrap();
        writer.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0;
             CREATE TABLE conversation_summaries(conversation_id TEXT PRIMARY KEY, workspace_uris TEXT);
             INSERT INTO conversation_summaries VALUES ('a', '[\"first\"]');"
        ).unwrap();
        let reader = open(&path, "readonly_shm=1").unwrap();
        let transaction = reader.unchecked_transaction().unwrap();
        assert_eq!(
            query(&transaction, "a").unwrap().as_deref(),
            Some(r#"["first"]"#)
        );

        writer
            .execute_batch("UPDATE conversation_summaries SET workspace_uris = '[\"second\"]'")
            .unwrap();
        let (busy, frames, checkpointed): (u32, u32, u32) = writer
            .query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })
            .unwrap();
        assert_eq!(busy, 0);
        assert!(
            checkpointed < frames,
            "active snapshot should pin a pre-commit frame"
        );
        assert_eq!(
            query(&transaction, "a").unwrap().as_deref(),
            Some(r#"["first"]"#)
        );
        transaction.commit().unwrap();
        assert_eq!(
            query(&reader, "a").unwrap().as_deref(),
            Some(r#"["second"]"#)
        );
        drop(reader);
        writer.close().unwrap();
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir(root).unwrap();
    }
}
