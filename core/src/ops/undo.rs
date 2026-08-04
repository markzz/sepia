use std::fs;
use std::path::PathBuf;

use super::{copy_recursive, delete_permanently, move_path};

/// How to recreate an item that `UndoStep::Remove` deleted, if its removal
/// is later undone (or redone, in the copy case).
#[derive(Clone)]
pub enum Restore {
    EmptyFolder,
    CopyFrom(PathBuf),
}

/// A single reversible filesystem step. `perform` executes the step and
/// returns its own inverse, ready to be registered as the next undo/redo
/// action — this is what lets one registration toggle back and forth
/// indefinitely between undo and redo.
///
/// Deliberately excludes permanent delete: matching Nautilus's own
/// convention, permanent delete is never undoable, only trash moves are.
#[derive(Clone)]
pub enum UndoStep {
    Remove { path: PathBuf, restore: Restore },
    Restore { path: PathBuf, restore: Restore },
    Rename { from: PathBuf, to: PathBuf },
    Move { from: PathBuf, to: PathBuf },
    /// Several steps performed and undone/redone together as one unit —
    /// e.g. a batch rename, so a single Undo reverts every item at once
    /// rather than needing one Undo per file.
    Batch(Vec<UndoStep>),
}

impl UndoStep {
    pub fn perform(&self) -> Result<UndoStep, String> {
        match self {
            UndoStep::Remove { path, restore } => {
                delete_permanently(path).map_err(|e| e.to_string())?;
                Ok(UndoStep::Restore {
                    path: path.clone(),
                    restore: restore.clone(),
                })
            }
            UndoStep::Restore { path, restore } => {
                match restore {
                    Restore::EmptyFolder => fs::create_dir(path),
                    Restore::CopyFrom(src) => copy_recursive(src, path),
                }
                .map_err(|e| e.to_string())?;
                Ok(UndoStep::Remove {
                    path: path.clone(),
                    restore: restore.clone(),
                })
            }
            UndoStep::Rename { from, to } => {
                fs::rename(from, to).map_err(|e| e.to_string())?;
                Ok(UndoStep::Rename {
                    from: to.clone(),
                    to: from.clone(),
                })
            }
            UndoStep::Move { from, to } => {
                move_path(from, to).map_err(|e| e.to_string())?;
                Ok(UndoStep::Move {
                    from: to.clone(),
                    to: from.clone(),
                })
            }
            UndoStep::Batch(steps) => {
                let mut inverses = Vec::with_capacity(steps.len());
                for step in steps {
                    inverses.push(step.perform()?);
                }
                // Reverse order in case later steps depended on earlier
                // ones (e.g. a rename into a path an earlier rename just
                // vacated) — undoing must unwind in the opposite order.
                inverses.reverse();
                Ok(UndoStep::Batch(inverses))
            }
        }
    }
}
