//! The linear-memory arena for `-- @own` tables.
//!
//! Owned sequences live here for the program's whole duration —
//! there is no GC and no reuse. Ownership is enforced by the
//! interpreter: moves invalidate the source, references are
//! read-only, and owned values cannot escape their scope.

use crate::value::Value;

/// One arena: the program's linear memory. Slots are never reused —
/// allocation is a bump.
#[derive(Debug, Default)]
pub struct Arenas {
    slots: Vec<Option<Vec<Value>>>,
}

/// A handle to one owned table in the arena. Copyable, but the
/// interpreter enforces single-owner semantics around it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct OwnHandle(pub usize);

impl Arenas {
    /// Allocates a sequence into the arena; the values become the
    /// elements at 1-based indices.
    pub fn alloc(&mut self, items: Vec<Value>) -> OwnHandle {
        self.slots.push(Some(items));
        OwnHandle(self.slots.len() - 1)
    }

    pub fn len(&self, handle: OwnHandle) -> usize {
        self.slot(handle).len()
    }

    /// The 1-based element, or `nil` past the end — Lua reads.
    pub fn get(&self, handle: OwnHandle, index: i64) -> Value {
        if index < 1 {
            return Value::Nil;
        }
        self.slot(handle)
            .get((index - 1) as usize)
            .cloned()
            .unwrap_or(Value::Nil)
    }

    /// Writes the 1-based element. Writing one past the end grows;
    /// writing further grows with nils filling the gap (Lua tables
    /// are sparse; the arena models that with explicit nils).
    pub fn set(&mut self, handle: OwnHandle, index: i64, value: Value) -> Result<(), String> {
        if index < 1 {
            return Err("array index starts at 1".to_string());
        }
        let slot = self.slot_mut(handle);
        let i = (index - 1) as usize;
        if i >= slot.len() {
            slot.resize(i + 1, Value::Nil);
        }
        slot[i] = value;
        Ok(())
    }

    /// The elements in order — `pairs`/`ipairs`/`#` iterate these.
    pub fn items(&self, handle: OwnHandle) -> Vec<Value> {
        self.slot(handle).clone()
    }

    fn slot(&self, handle: OwnHandle) -> &Vec<Value> {
        self.slots
            .get(handle.0)
            .and_then(|s| s.as_ref())
            .expect("owned table dropped from the arena")
    }

    fn slot_mut(&mut self, handle: OwnHandle) -> &mut Vec<Value> {
        self.slots
            .get_mut(handle.0)
            .and_then(|s| s.as_mut())
            .expect("owned table dropped from the arena")
    }
}
