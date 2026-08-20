use crate::error::Error;
use crate::error::Result;
use objc2::rc::Retained;
use objc2_foundation::{
   NSArray, NSError, NSNumber, NSString, NSURL, NSURLIsExcludedFromBackupKey, NSUnderlyingErrorKey,
};
use std::path::Path;

/// Marks `path` as excluded from the platform's backup system.
///
/// Sets [`NSURLIsExcludedFromBackupKey`], keeping `path` out of iCloud Backup on iOS and
/// Time Machine on macOS. Purgeability follows from the location, not this flag, so pair it
/// with `ApplicationSupportDirectory` rather than `CachesDirectory`.
///
/// `path` must already exist, and is best resolved through
/// [`PathResolver`](crate::PathResolver): relative paths resolve against the process
/// working directory, `~` is not expanded, symlinks are followed, and nothing confines
/// `path` to your own app. `excluded: false` clears the flag, and since the flag lives on
/// the item, recreating a directory drops it — reapply, and check with
/// [`is_excluded_from_backup`].
///
/// iOS and macOS only; elsewhere call sites need a `cfg` gate. Android uses a location
/// instead ([`AndroidPath::NoBackupFilesDir`](crate::AndroidPath::NoBackupFilesDir)).
///
/// [`NSURLIsExcludedFromBackupKey`]: https://developer.apple.com/documentation/foundation/nsurlisexcludedfrombackupkey
pub fn set_excluded_from_backup(path: &Path, excluded: bool) -> Result<()> {
   let url = file_url(path)?;
   let value = NSNumber::new_bool(excluded);

   // SAFETY: `NSURLIsExcludedFromBackupKey` is an immutable Foundation constant
   // initialized before any Rust code runs, so the `extern "C"` read is sound. The setter
   // is unsafe only because the binding cannot type-check the value against its key; this
   // key takes a boolean `NSNumber`, which is all `NSNumber::new_bool` produces, and
   // `value` stays retained across the call.
   unsafe { url.setResourceValue_forKey_error(Some(&value), NSURLIsExcludedFromBackupKey) }.map_err(
      |error| Error::BackupExclusionFailed {
         path: format!("{path:?}"),
         reason: format_ns_error(&error),
      },
   )
}

/// Reports whether `path` is marked as excluded from the platform's backup system.
///
/// A missing `path` reads as `false` — unlike [`set_excluded_from_backup`], there is no
/// existence precondition. One unreachable through its parent errors instead, as does a
/// trailing slash on a file: the check reads it as naming a directory, where
/// `set_excluded_from_backup` normalises the slash away.
pub fn is_excluded_from_backup(path: &Path) -> Result<bool> {
   // Must be a fresh `NSURL`: resource values are cached on first read, so a long-lived
   // instance goes stale against a change made out of band.
   let url = file_url(path)?;

   // Foundation reports an unreachable item as not excluded rather than as an error, which
   // would make a `false` mean "cannot tell". `metadata` follows symlinks, matching where
   // the flag is set; `symlink_metadata` would succeed on a link into an unreadable parent.
   match std::fs::metadata(path) {
      Ok(_) => {}
      Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
      Err(error) => {
         return Err(Error::BackupExclusionFailed {
            path: format!("{path:?}"),
            reason: error.to_string(),
         });
      }
   }

   // SAFETY: as in `set_excluded_from_backup`.
   let key = unsafe { NSURLIsExcludedFromBackupKey };

   let values = url
      .resourceValuesForKeys_error(&NSArray::from_slice(&[key]))
      .map_err(|error| Error::BackupExclusionFailed {
         path: format!("{path:?}"),
         reason: format_ns_error(&error),
      })?;

   Ok(values
      .objectForKey(key)
      .and_then(|value| value.downcast_ref::<NSNumber>().map(NSNumber::as_bool))
      .unwrap_or(false))
}

/// Builds a file URL for `path`.
///
/// Uses `fileURLWithPath:`, which reads directory-ness from the filesystem, rather than
/// `fileURLWithPath:isDirectory:`, which would mean asserting something already known.
fn file_url(path: &Path) -> Result<Retained<NSURL>> {
   let path_string = path
      .to_str()
      .ok_or_else(|| Error::InvalidPath(format!("Path is not valid UTF-8: {path:?}")))?;

   // `fileURLWithPath:` returns nil for both of these, and the binding's return type is
   // not optional, so objc2 would panic instead of yielding an error.
   if path_string.is_empty() {
      return Err(Error::InvalidPath("Path is empty".to_string()));
   }

   if path_string.contains('\0') {
      return Err(Error::InvalidPath(format!(
         "Path contains an interior NUL: {:?}",
         path
      )));
   }

   Ok(NSURL::fileURLWithPath(&NSString::from_str(path_string)))
}

/// Renders an `NSError`, including any underlying error.
///
/// The localized description alone can misattribute the cause -- a permission denial
/// reports as a read-only volume -- so the domain, code and underlying error are carried
/// too. Descriptions are escaped: Foundation embeds the filename, which may contain a
/// newline.
fn format_ns_error(error: &NSError) -> String {
   let mut message = format!(
      "{:?} (domain {}, code {})",
      error.localizedDescription().to_string(),
      error.domain(),
      error.code()
   );

   // SAFETY: as above. Read from `userInfo` rather than via `underlyingErrors`, which does
   // not exist before macOS 11.3 / iOS 14.5.
   let underlying_key = unsafe { NSUnderlyingErrorKey };

   let user_info = error.userInfo();

   if let Some(value) = user_info.objectForKey(underlying_key)
      && let Some(underlying) = value.downcast_ref::<NSError>()
   {
      message.push_str(&format!(
         "; underlying: {:?} (domain {}, code {})",
         underlying.localizedDescription().to_string(),
         underlying.domain(),
         underlying.code()
      ));
   }

   message
}

#[cfg(test)]
// CI runs these on macos-latest; the iOS build of the same code is only compile-checked.
// So they prove the binding and read-back work on macOS, and no more: not sandbox
// enforcement, and not whether iCloud Backup or Time Machine honours the flag.
mod tests {
   use super::*;
   use std::path::PathBuf;
   use std::sync::atomic::{AtomicUsize, Ordering};

   /// Stands in for `tempfile::TempDir`, which this crate does not depend on. Dropping
   /// removes the tree, so a failed assertion does not leak it.
   struct TempDir {
      path: PathBuf,
   }

   impl TempDir {
      fn new(label: &str) -> Self {
         static COUNTER: AtomicUsize = AtomicUsize::new(0);

         // Pid plus counter keeps the name unique under the parallel harness.
         let path = std::env::temp_dir().join(format!(
            "fs-resolver-{}-{}-{}",
            label,
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
         ));

         // `create_dir`, not `create_dir_all`: a directory left behind by a killed run
         // would otherwise be adopted along with its flag. Collisions should be loud.
         std::fs::create_dir(&path).expect("creating the temp directory should succeed");

         Self { path }
      }

      fn path(&self) -> &Path {
         &self.path
      }
   }

   impl Drop for TempDir {
      fn drop(&mut self) {
         let _ = std::fs::remove_dir_all(&self.path);
      }
   }

   #[test]
   fn set_excluded_from_backup_round_trips_the_flag() {
      let dir = TempDir::new("round-trip");
      assert!(!is_excluded_from_backup(dir.path()).unwrap());

      set_excluded_from_backup(dir.path(), true).unwrap();
      assert!(is_excluded_from_backup(dir.path()).unwrap());

      // Repeating the call leaves it set.
      set_excluded_from_backup(dir.path(), true).unwrap();
      assert!(is_excluded_from_backup(dir.path()).unwrap());

      set_excluded_from_backup(dir.path(), false).unwrap();
      assert!(!is_excluded_from_backup(dir.path()).unwrap());
   }

   #[test]
   fn set_excluded_from_backup_marks_a_file_as_well_as_a_directory() {
      let dir = TempDir::new("file");
      let file = dir.path().join("regenerable.bin");
      std::fs::write(&file, b"contents").unwrap();

      set_excluded_from_backup(&file, true).unwrap();
      assert!(is_excluded_from_backup(&file).unwrap());
   }

   #[test]
   fn set_excluded_from_backup_fails_for_a_missing_path() {
      let dir = TempDir::new("missing");
      let missing = dir.path().join("does-not-exist");

      // Only the variant and path: the reason text varies by OS version and locale.
      let error = set_excluded_from_backup(&missing, true).unwrap_err();
      assert!(
         matches!(&error, Error::BackupExclusionFailed { path, .. } if path == &format!("{missing:?}")),
         "unexpected error: {error:?}"
      );
   }

   #[test]
   fn is_excluded_from_backup_reads_false_for_a_missing_path() {
      let dir = TempDir::new("missing-read");

      // Not an error: a nonexistent item is not excluded. `set_excluded_from_backup`
      // does error here, and that asymmetry is deliberate.
      assert!(!is_excluded_from_backup(&dir.path().join("does-not-exist")).unwrap());
   }

   #[test]
   fn all_functions_reject_an_empty_path_instead_of_panicking() {
      // Without the guard in `file_url` these panic rather than return.
      let empty = PathBuf::from("");

      for error in [
         set_excluded_from_backup(&empty, true).unwrap_err(),
         is_excluded_from_backup(&empty).unwrap_err(),
      ] {
         assert!(
            matches!(&error, Error::InvalidPath(_)),
            "unexpected error: {error:?}"
         );
      }
   }

   #[test]
   fn set_excluded_from_backup_rejects_a_path_with_an_interior_nul() {
      let error = set_excluded_from_backup(&PathBuf::from("/tmp/a\0b"), true).unwrap_err();
      assert!(
         matches!(&error, Error::InvalidPath(_)),
         "unexpected error: {error:?}"
      );
   }

   #[test]
   fn set_excluded_from_backup_rejects_a_path_that_is_not_utf8() {
      use std::os::unix::ffi::OsStrExt;

      // Rejected before any filesystem access, so no temp directory is needed.
      let path = PathBuf::from(std::ffi::OsStr::from_bytes(&[0xff, 0xfe]));

      let error = set_excluded_from_backup(&path, true).unwrap_err();
      assert!(
         matches!(&error, Error::InvalidPath(_)),
         "unexpected error: {error:?}"
      );
   }

   #[test]
   fn is_excluded_from_backup_errors_when_the_parent_cannot_be_traversed() {
      use std::os::unix::fs::PermissionsExt;

      let dir = TempDir::new("unreadable");
      let parent = dir.path().join("locked");
      let target = parent.join("generated");
      std::fs::create_dir_all(&target).unwrap();
      set_excluded_from_backup(&target, true).unwrap();
      assert!(is_excluded_from_backup(&target).unwrap());

      std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o000)).unwrap();

      // Foundation would report this excluded item as `false`; the probe turns it into an
      // error so a `false` never means "could not tell".
      let result = is_excluded_from_backup(&target);

      // Capture the precondition while it still holds. Running as root, or on a file system
      // that ignores permission bits, the read succeeds and there is nothing to assert --
      // but asserting only inside an `if let Err` would let a deleted probe pass silently.
      let denied = std::fs::metadata(&target).is_err();

      // Restore before asserting, so a failure still lets `TempDir` clean up.
      std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o700)).unwrap();

      if !denied {
         return;
      }

      let error = result.expect_err("an unreadable item must error, not read as false");
      assert!(
         matches!(&error, Error::BackupExclusionFailed { path, .. } if path == &format!("{target:?}")),
         "unexpected error: {error:?}"
      );
   }

   #[test]
   fn set_excluded_from_backup_follows_a_symlink_to_its_target() {
      let dir = TempDir::new("symlink");
      let target = dir.path().join("real");
      let link = dir.path().join("link");
      std::fs::create_dir(&target).unwrap();
      std::os::unix::fs::symlink(&target, &link).unwrap();

      set_excluded_from_backup(&link, true).unwrap();
      assert!(is_excluded_from_backup(&target).unwrap());
   }

   #[test]
   fn exclusion_is_lost_when_the_directory_is_recreated() {
      // The flag belongs to the item, so a delete-and-recreate drops it silently. This is
      // why `is_excluded_from_backup` exists.
      let dir = TempDir::new("recreated");
      let target = dir.path().join("generated");
      std::fs::create_dir(&target).unwrap();

      set_excluded_from_backup(&target, true).unwrap();
      assert!(is_excluded_from_backup(&target).unwrap());

      std::fs::remove_dir_all(&target).unwrap();
      std::fs::create_dir(&target).unwrap();
      assert!(!is_excluded_from_backup(&target).unwrap());

      // Reapplying restores it.
      set_excluded_from_backup(&target, true).unwrap();
      assert!(is_excluded_from_backup(&target).unwrap());
   }
}
