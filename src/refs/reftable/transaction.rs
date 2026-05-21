//! `ReftableStore` transaction: buffer updates in memory; on commit write a
//! single new table file and atomically append to `tables.list`.
//!
//! Spec reference §5 — update transactions:
//!   1. Acquire `tables.list.lock`.
//!   2. Read `tables.list` to determine current reftables.
//!   3. Select `update_index = max(prior max_update_index) + 1`.
//!   4. Prepare temp reftable, including log entries.
//!   5. Rename temp to `${min}-${max}-${random}.ref`.
//!   6. Copy `tables.list` to `tables.list.lock`, appending file from (5).
//!   7. Rename `tables.list.lock` to `tables.list`.

use std::path::PathBuf;

use crate::hash::ObjectId;

use super::super::reflog::Identity;
use super::super::{
    ExpectedOldValue, FullName, NewValue, RefError, RefStore, RefTarget, RefTransactionTrait,
    RefUpdateError, ReflogMessage,
};
use super::writer::{
    make_table_filename, random_suffix, write_table_file, TableUpdate, WriteLogEntry, WriteRefValue,
};
use super::ReftableStore;

#[derive(Debug)]
enum Op {
    Update {
        name: FullName,
        expected: ExpectedOldValue,
        new: NewValue,
        reflog: ReflogMessage,
    },
    Delete {
        name: FullName,
        expected: ExpectedOldValue,
    },
}

pub struct ReftableTransaction<'a> {
    store: &'a ReftableStore,
    ops: Vec<Op>,
    identity: Identity,
}

impl<'a> ReftableTransaction<'a> {
    pub fn new(store: &'a ReftableStore) -> Self {
        Self {
            store,
            ops: Vec::new(),
            identity: Identity::from_env_or_placeholder(),
        }
    }
}

impl<'a> RefTransactionTrait for ReftableTransaction<'a> {
    fn update(
        &mut self,
        name: &FullName,
        expected: ExpectedOldValue,
        new: NewValue,
        reflog: ReflogMessage,
    ) -> Result<(), RefError> {
        self.ops.push(Op::Update {
            name: name.clone(),
            expected,
            new,
            reflog,
        });
        Ok(())
    }

    fn delete(&mut self, name: &FullName, expected: ExpectedOldValue) -> Result<(), RefError> {
        self.ops.push(Op::Delete {
            name: name.clone(),
            expected,
        });
        Ok(())
    }

    fn commit(self: Box<Self>) -> Result<(), RefError> {
        let me = *self;
        let ReftableTransaction {
            store,
            ops,
            identity,
        } = me;
        if ops.is_empty() {
            return Ok(());
        }

        // Spec §5 step 1: serialize within-process; acquire tables.list.lock.
        let _write_guard = store.write_guard();
        let lock_path = store.reftable_dir().join("tables.list.lock");
        let list_path = store.reftable_dir().join("tables.list");

        // Acquire the on-disk lock.
        let lock_file = match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock_path)
        {
            Ok(f) => f,
            Err(e) => {
                return Err(RefError::Io {
                    path: lock_path,
                    source: e,
                })
            }
        };
        drop(lock_file);

        // From here we MUST release the on-disk lock on every exit path.
        let result = (|| -> Result<(), RefError> {
            // Read existing tables.list to determine next update_index.
            let existing_text = match std::fs::read_to_string(&list_path) {
                Ok(s) => s,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
                Err(e) => {
                    return Err(RefError::Io {
                        path: list_path.clone(),
                        source: e,
                    })
                }
            };
            let mut existing_lines: Vec<String> = existing_text
                .lines()
                .map(|s| s.to_string())
                .filter(|s| !s.is_empty())
                .collect();

            // Compute next update_index = max(prior max) + 1.
            store.refresh()?; // re-read stack with the just-loaded tables.list
            let prior_max = {
                let guard = store.stack.read().expect("stack lock poisoned");
                guard
                    .tables()
                    .iter()
                    .map(|t| t.header().max_update_index)
                    .max()
                    .unwrap_or(0)
            };
            let update_index = prior_max + 1;

            // Validate expected-old-value AND translate ops to writer updates.
            let mut writer_updates: Vec<TableUpdate> = Vec::new();
            // Cache current values so we can populate reflog old_oid.
            let now_secs = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            // We mirror the loose backend's behavior of also writing a HEAD
            // reflog entry when HEAD symbolically points at the updated ref.
            // To do that, scan the ops for the HEAD symref target first.
            let head_name = FullName::new("HEAD").expect("HEAD is always valid");
            let head_target: Option<FullName> = match store.read(&head_name) {
                Ok(Some(r)) => match r.target {
                    RefTarget::Symbolic(t) => Some(t),
                    _ => None,
                },
                _ => None,
            };

            // Group HEAD reflog mirroring entries.
            let mut head_mirrors: Vec<TableUpdate> = Vec::new();

            for op in ops {
                match op {
                    Op::Update {
                        name,
                        expected,
                        new,
                        reflog,
                    } => {
                        let current = current_direct(store, &name)?;
                        validate_expected(&name, &expected, current)?;

                        let value = match &new {
                            NewValue::Direct(oid) => WriteRefValue::Direct(*oid),
                            NewValue::Symbolic(t) => WriteRefValue::Symbolic(t.clone()),
                        };
                        let reflog_entry = match (&reflog.0, &new) {
                            (Some(msg), NewValue::Direct(new_oid)) => Some(WriteLogEntry {
                                old_oid: current
                                    .unwrap_or_else(|| ObjectId::null(store.hash_kind())),
                                new_oid: *new_oid,
                                committer_name: identity.name.clone(),
                                committer_email: identity.email.clone(),
                                time_seconds: now_secs,
                                tz_offset_minutes: local_tz_offset_minutes(),
                                message: sanitize_msg(msg),
                            }),
                            _ => None,
                        };
                        // HEAD mirror.
                        if let (Some(msg), NewValue::Direct(new_oid)) = (&reflog.0, &new) {
                            if name != head_name {
                                if let Some(target) = &head_target {
                                    if target == &name {
                                        head_mirrors.push(TableUpdate {
                                            name: head_name.clone(),
                                            // Don't change HEAD itself, just add a reflog entry.
                                            // We do this by emitting a value
                                            // that matches HEAD's current symref pointer.
                                            value: WriteRefValue::Symbolic(target.clone()),
                                            reflog: Some(WriteLogEntry {
                                                old_oid: current.unwrap_or_else(|| {
                                                    ObjectId::null(store.hash_kind())
                                                }),
                                                new_oid: *new_oid,
                                                committer_name: identity.name.clone(),
                                                committer_email: identity.email.clone(),
                                                time_seconds: now_secs,
                                                tz_offset_minutes: local_tz_offset_minutes(),
                                                message: sanitize_msg(msg),
                                            }),
                                        });
                                    }
                                }
                            }
                        }
                        writer_updates.push(TableUpdate {
                            name,
                            value,
                            reflog: reflog_entry,
                        });
                    }
                    Op::Delete { name, expected } => {
                        let current = current_direct(store, &name)?;
                        validate_expected(&name, &expected, current)?;
                        writer_updates.push(TableUpdate {
                            name,
                            value: WriteRefValue::Deletion,
                            reflog: None,
                        });
                    }
                }
            }
            // Append HEAD mirror entries; dedup by name (writer sorts anyway).
            writer_updates.extend(head_mirrors);

            // Generate filename.
            let suffix = random_suffix();
            let filename = make_table_filename(update_index, update_index, &suffix);
            let new_table_path: PathBuf = store.reftable_dir().join(&filename);
            write_table_file(
                &new_table_path,
                store.hash_kind(),
                update_index,
                writer_updates,
            )?;

            // Update tables.list under the lock: write new list to a temp,
            // rename over original.
            existing_lines.push(filename);
            let mut buf = String::new();
            for line in &existing_lines {
                buf.push_str(line);
                buf.push('\n');
            }
            std::fs::write(&lock_path, buf.as_bytes()).map_err(|e| RefError::Io {
                path: lock_path.clone(),
                source: e,
            })?;
            std::fs::rename(&lock_path, &list_path).map_err(|e| RefError::Io {
                path: list_path.clone(),
                source: e,
            })?;

            Ok(())
        })();

        // If the lock file still exists (i.e., we failed before the
        // rename), clean it up.
        let _ = std::fs::remove_file(&lock_path);
        // Refresh the in-memory stack so subsequent reads see the new table.
        store.refresh()?;
        result
    }
}

fn current_direct(store: &ReftableStore, name: &FullName) -> Result<Option<ObjectId>, RefError> {
    // Walk symbolic chain (max depth 5, matching `RefTarget::resolve`).
    let mut name = name.clone();
    for _ in 0..5 {
        match store.read(&name)? {
            None => return Ok(None),
            Some(r) => match r.target {
                RefTarget::Direct(o) => return Ok(Some(o)),
                RefTarget::Symbolic(next) => name = next,
            },
        }
    }
    Err(RefError::SymbolicCycle(name.into_string()))
}

fn validate_expected(
    name: &FullName,
    expected: &ExpectedOldValue,
    current: Option<ObjectId>,
) -> Result<(), RefError> {
    match (expected, current) {
        (ExpectedOldValue::Any, _) => Ok(()),
        (ExpectedOldValue::Missing, Some(_)) => {
            Err(RefUpdateError::ExpectedMissing(name.to_string()).into())
        }
        (ExpectedOldValue::Missing, None) => Ok(()),
        (ExpectedOldValue::Direct(_), None) => {
            Err(RefUpdateError::OldValueMismatch(name.to_string()).into())
        }
        (ExpectedOldValue::Direct(want), Some(got)) if *want != got => {
            Err(RefUpdateError::OldValueMismatch(name.to_string()).into())
        }
        (ExpectedOldValue::Direct(_), Some(_)) => Ok(()),
    }
}

fn sanitize_msg(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c == '\n' || c == '\r' || c == '\t' {
                ' '
            } else {
                c
            }
        })
        .collect()
}

#[cfg(unix)]
fn local_tz_offset_minutes() -> i16 {
    use std::process::Command;
    if let Ok(out) = Command::new("date").arg("+%z").output() {
        if let Ok(s) = std::str::from_utf8(&out.stdout) {
            let s = s.trim();
            if s.len() == 5 {
                let sign: i16 = if s.starts_with('-') { -1 } else { 1 };
                if let (Ok(hh), Ok(mm)) = (s[1..3].parse::<i16>(), s[3..5].parse::<i16>()) {
                    return sign * (hh * 60 + mm);
                }
            }
        }
    }
    0
}

#[cfg(not(unix))]
fn local_tz_offset_minutes() -> i16 {
    0
}
