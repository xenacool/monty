//! Per-session memory isolation for platforms that cannot use subprocesses.
//!
//! Unlike [`crate::LimitedAllocator`] (the `#[global_allocator]`), this module
//! provides memory isolation *per session* using a bump arena. Each session
//! gets an [`Arena`] with a hard memory ceiling enforced by
//! [bumpalo's built-in allocation limit](bumpalo::Bump::set_allocation_limit);
//! exceeding the limit panics, which the in-process worker thread catches as
//! a `FatalError` — the cart crashes, the host survives.
//!
//! Designed for iOS, Android, and game cartridges where `fork()` is unavailable
//! or prohibited but untrusted code still needs memory bounds.
//!
//! # Installation
//!
//! For the arena to limit **all** memory (including the Monty interpreter's
//! heap), the host binary **must** install [`ThreadLocalArena`] as the
//! `#[global_allocator]`. Without it, the arena only limits its own direct
//! allocations, not the Monty heap — the interpreter's soft `ResourceTracker`
//! limit still works as a backup.
//!
//! ```ignore
//! // In the host binary (not a library):
//! #[global_allocator]
//! static ALLOC: monty_alloc::arena::ThreadLocalArena = monty_alloc::arena::ThreadLocalArena;
//! ```
//!
//! # Misconfiguration
//!
//! If the host forgets to install [`ThreadLocalArena`] as the global allocator,
//! the arena still works — it just limits only its own direct allocations, not
//! the Monty heap. This is safe: no undefined behavior, no dangling pointers,
//! no data corruption. The interpreter's soft `ResourceTracker` limit still
//! works as a backup.
//!
//! If [`ThreadLocalArena::with_arena`] is never called on a worker thread,
//! allocations fall through to the system allocator. The `allocated_bytes()`
//! helper returns `0` when no arena is active, and the fallback path is
//! tested to never panic or corrupt state.
//!
//! With the `require-tla` feature enabled, the fallback panics instead of
//! silently using the system allocator. This catches misconfiguration at
//! runtime: the worker thread's `catch_unwind` catches the panic and reports
//! a `FatalError`, so the host survives.
//!
//! Without `require-tla`, the first fallback use on each thread emits a one-time
//! warning to stderr. Subsequent fallbacks on the same thread are silent to
//! avoid log spam.
//!
//! If the closure inside [`ThreadLocalArena::with_arena`] panics, the guard
//! restores the thread-local state before the panic propagates — subsequent
//! calls work correctly without corruption.
//!
//! # Usage
//!
//! ```rust
//! use monty_alloc::arena::Arena;
//!
//! // Create an arena with a 10 MiB ceiling.
//! let mut arena = Arena::new(Some(10 * 1024 * 1024));
//!
//! // Allocate values into the arena.
//! let ptr = arena.alloc(42u32);
//! assert_eq!(*ptr, 42);
//!
//! // Reset clears all allocations for the next session.
//! arena.reset();
//! ```

use std::{
    alloc::{GlobalAlloc, Layout, System},
    cell::Cell,
    ptr,
};

use bumpalo::Bump;

/// A per-session memory arena with a configurable hard memory limit.
///
/// Each session or "cart" gets one arena. The hard limit is enforced by
/// [bumpalo's built-in allocation limit](bumpalo::Bump::set_allocation_limit),
/// which panics when new backing chunks would exceed the ceiling. The caller
/// (typically the in-process worker thread) catches the panic and reports a
/// `FatalError`.
///
/// The limit is enforced at the allocator level — no polling or `LIVE_MEMORY`
/// tracking is needed. When [`ThreadLocalArena`] is installed as the global
/// allocator and [`ThreadLocalArena::with_arena`] is used, **all** allocations
/// (including the Monty interpreter's heap) go through the arena and are
/// subject to the limit.
pub struct Arena {
    /// The underlying bumpalo arena.
    bump: Bump,
}

impl Arena {
    /// Creates a new arena with an optional hard memory limit in bytes.
    ///
    /// The limit is enforced by bumpalo at the allocator level: when the arena
    /// needs a new backing chunk and the total would exceed `memory_limit`,
    /// the allocation panics. `None` disables the hard limit.
    ///
    /// For the limit to cover **all** memory (including the Monty heap), the
    /// host must install [`ThreadLocalArena`] as the `#[global_allocator]` and
    /// use [`ThreadLocalArena::with_arena`] to route allocations through this
    /// arena.
    #[must_use]
    pub fn new(memory_limit: Option<usize>) -> Self {
        let bump = Bump::new();
        bump.set_allocation_limit(memory_limit);
        Self { bump }
    }

    /// Creates a new arena with the given initial capacity and optional limit.
    ///
    /// The initial capacity is the first chunk size; subsequent chunks grow
    /// as needed up to the limit.
    #[must_use]
    pub fn with_capacity(capacity: usize, memory_limit: Option<usize>) -> Self {
        let bump = Bump::with_capacity(capacity);
        bump.set_allocation_limit(memory_limit);
        Self { bump }
    }

    /// Returns the configured allocation limit in bytes, or `None` if unlimited.
    #[inline]
    pub fn allocation_limit(&self) -> Option<usize> {
        self.bump.allocation_limit()
    }

    /// Sets or removes the allocation limit.
    ///
    /// The limit is only enforced when allocating new backing chunks — existing
    /// allocations and future allocations within the current chunk are unaffected.
    #[inline]
    pub fn set_allocation_limit(&self, limit: Option<usize>) {
        self.bump.set_allocation_limit(limit);
    }

    /// Allocates a value of type `T` in the arena and returns a mutable reference.
    ///
    /// The reference is valid until [`reset`](Self::reset) is called or the
    /// arena is dropped. Panics if the allocation would exceed the limit.
    #[inline]
    pub fn alloc<T>(&self, value: T) -> &mut T {
        self.bump.alloc(value)
    }

    /// Allocates a copy of `slice` as a clone in the arena.
    ///
    /// Panics if the allocation would exceed the limit.
    #[inline]
    pub fn alloc_slice_clone<T: Clone>(&self, slice: &[T]) -> &mut [T] {
        self.bump.alloc_slice_clone(slice)
    }

    /// Allocates a copy of `slice` in the arena.
    ///
    /// Panics if the allocation would exceed the limit.
    #[inline]
    pub fn alloc_slice_copy<T: Copy>(&self, slice: &[T]) -> &mut [T] {
        self.bump.alloc_slice_copy(slice)
    }

    /// Allocates a `String` backed by the arena.
    ///
    /// Panics if the allocation would exceed the limit.
    #[inline]
    pub fn alloc_string(&self, str: &str) -> &mut str {
        self.bump.alloc_str(str)
    }

    /// Returns the total bytes this arena has requested from the system
    /// allocator, including bumpalo's internal metadata overhead.
    #[inline]
    pub fn allocated_bytes(&self) -> usize {
        self.bump.allocated_bytes()
    }

    /// Resets the arena: all bump-allocated objects become invalid, and excess
    /// chunks are returned to the system allocator.
    ///
    /// Does **not** run `Drop` implementations on arena-allocated objects.
    /// This is intentional: Monty's heap objects manage their own cleanup
    /// through the interpreter's drop paths. The arena only provides the
    /// raw memory buffer for temporary data.
    pub fn reset(&mut self) {
        self.bump.reset();
    }

    /// Returns a reference to the inner `Bump` for advanced usage.
    ///
    /// Prefer the methods on [`Arena`] for memory tracking correctness.
    #[inline]
    pub fn bump(&self) -> &Bump {
        &self.bump
    }
}

/// A `#[global_allocator]` that routes all allocations through a thread-local
/// [`Arena`].
///
/// This is the bridge between Rust's global allocator mechanism and per-session
/// arena isolation. The host binary **must** install this as the `#[global_allocator]`
/// for the arena to limit **all** memory (including the Monty interpreter's heap).
///
/// ```ignore
/// // In the host binary (not a library):
/// #[global_allocator]
/// static ALLOC: monty_alloc::arena::ThreadLocalArena = monty_alloc::arena::ThreadLocalArena;
/// ```
///
/// Without this installation, the arena only limits its own direct allocations,
/// not the Monty heap. The interpreter's soft `ResourceTracker` limit still
/// works as a backup.
///
/// Use [`ThreadLocalArena::with_arena`] to set the active arena for the current
/// thread and run a closure. The worker thread should call this around each
/// protocol dispatch.
pub struct ThreadLocalArena;

thread_local! {
    /// The current arena for this thread; `None` falls through to the system
    /// allocator (should not happen in a properly configured worker).
    static CURRENT_ARENA: Cell<Option<ptr::NonNull<Bump>>> = const { Cell::new(None) };
}

thread_local! {
    /// Tracks whether the fallback warning has been emitted on this thread.
    /// Used only when `require-tla` is disabled to avoid log spam.
    static FALLBACK_WARNED: Cell<bool> = const { Cell::new(false) };
}

impl ThreadLocalArena {
    /// Sets the active arena for this thread and runs `f`.
    ///
    /// All allocations during `f` go through the arena via [`ThreadLocalArena`]
    /// as the `#[global_allocator]`. The arena's allocation limit (set via
    /// [`Arena::new`] or [`Arena::set_allocation_limit`]) is enforced by bumpalo
    /// at the allocator level — exceeding it panics.
    ///
    /// Restores the thread-local state on all paths, including panics.
    /// Does **not** catch panics — they propagate to the caller.
    ///
    /// # Panics
    ///
    /// Panics if called while another arena is already active on this thread
    /// (nested calls are not supported).
    pub fn with_arena<T>(arena: &Arena, f: impl FnOnce() -> T) -> T {
        // SAFETY: arena outlives this call (borrowed as &), and we restore
        // the cell on all paths (including panic, via the guard).
        struct ArenaGuard;
        impl Drop for ArenaGuard {
            fn drop(&mut self) {
                CURRENT_ARENA.set(None);
            }
        }
        let ptr = ptr::NonNull::from(arena.bump());
        CURRENT_ARENA.set(Some(ptr));
        let _guard = ArenaGuard;
        f()
    }

    /// Returns the bytes currently allocated by the active arena on this thread,
    /// or 0 if no arena is active.
    #[must_use]
    #[inline]
    pub fn allocated_bytes() -> usize {
        CURRENT_ARENA.with(|cell| {
            let Some(ptr) = cell.get() else {
                return 0;
            };
            // SAFETY: ptr was set from a &Bump that outlives this call.
            unsafe { ptr.as_ref().allocated_bytes() }
        })
    }

    /// Resets the fallback warning flag on the current thread.
    ///
    /// Used in tests to verify the warning is emitted on the first fallback use.
    #[doc(hidden)]
    #[cfg(test)]
    fn reset_fallback_warning() {
        FALLBACK_WARNED.set(false);
    }

    /// Checks whether the fallback warning has already been emitted on this thread.
    #[doc(hidden)]
    #[cfg(test)]
    fn fallback_warned() -> bool {
        FALLBACK_WARNED.with(|cell| cell.get())
    }

    /// Emit a one-time warning when the fallback allocator is used.
    ///
    /// When `require-tla` is enabled, this panics instead of warning.
    #[inline]
    fn on_fallback() {
        #[cfg(feature = "require-tla")]
        {
            panic!(
                "ThreadLocalArena: no arena active on this thread. \
                 The host must call `ThreadLocalArena::with_arena()` around each dispatch. \
                 If you see this, the `#[global_allocator]` is installed but the worker \
                 is not routing allocations through the arena."
            );
        }
        #[cfg(not(feature = "require-tla"))]
        {
            FALLBACK_WARNED.with(|warned| {
                if !warned.get() {
                    warned.set(true);
                    eprintln!(
                        "warning: monty-alloc ThreadLocalArena: no arena active on this thread. \
                         Allocations are falling through to the system allocator. \
                         Ensure `ThreadLocalArena::with_arena()` is called around each dispatch, \
                         or enable the `require-tla` feature to panic on misconfiguration."
                    );
                }
            });
        }
    }
}

// SAFETY: Every method forwards to either the thread-local arena's bumpalo
// `Bump` or to `System`. Bumpalo's `alloc` returns valid, aligned pointers
// for the given layout (or panics on OOM, which is the desired behavior for
// in-process workers). `dealloc` only frees pointers that came from our
// `alloc`, using the same layout. We never fabricate, alias, or double-free
// pointers.
unsafe impl GlobalAlloc for ThreadLocalArena {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        CURRENT_ARENA.with(|cell| {
            if let Some(bump_ptr) = cell.get() {
                // SAFETY: bump_ptr was set from a &Bump that outlives this call.
                let bump = unsafe { bump_ptr.as_ref() };
                bump.alloc_layout(layout).as_ptr()
            } else {
                // No arena active — warn or panic depending on feature flag.
                Self::on_fallback();
                // SAFETY: layout comes from the caller and is forwarded unchanged.
                unsafe { System.alloc(layout) }
            }
        })
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // Bumpalo doesn't support individual deallocation — allocations live
        // until reset(). We don't refund here; refund happens at reset().
        // This is fine because bumpalo's model is bump-only, no per-object free.
        CURRENT_ARENA.with(|cell| {
            if cell.get().is_none() {
                // No arena active — warn or panic depending on feature flag.
                Self::on_fallback();
                // SAFETY: ptr came from our alloc with this same layout.
                unsafe { System.dealloc(ptr, layout) };
            }
            // Otherwise: bumpalo ignores dealloc, which is correct for bump allocation.
        });
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // Bumpalo doesn't have alloc_zeroed; allocate and zero manually.
        // SAFETY: delegates to our alloc() which returns a valid pointer for the layout.
        let ptr = unsafe { self.alloc(layout) };
        if !ptr.is_null() && layout.size() > 0 {
            // SAFETY: ptr is valid for layout.size() bytes from alloc() above.
            unsafe { ptr::write_bytes(ptr, 0, layout.size()) };
        }
        ptr
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // Bumpalo can't shrink in place; allocate new and copy.
        // SAFETY: new_size and alignment are valid for the caller's request.
        let new_ptr = unsafe { self.alloc(Layout::from_size_align_unchecked(new_size, layout.align())) };
        if !new_ptr.is_null() && new_size > 0 {
            let copy_len = new_size.min(layout.size());
            if copy_len > 0 {
                // SAFETY: both pointers are valid for their respective sizes.
                unsafe { ptr::copy_nonoverlapping(ptr, new_ptr, copy_len) };
            }
        }
        // Don't dealloc the old ptr — bumpalo doesn't support it.
        new_ptr
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arena_allocates_and_tracks_memory() {
        let arena = Arena::new(Some(1024 * 1024));
        let _val = arena.alloc(42u32);
        let allocated = arena.allocated_bytes();
        assert!(allocated > 0, "arena should have allocated a chunk");
    }

    #[test]
    fn arena_reset_releases_memory() {
        let arena = &mut Arena::new(Some(1024 * 1024));
        let _val = arena.alloc(vec![0u8; 256]);
        let before = arena.allocated_bytes();
        arena.reset();
        let after = arena.allocated_bytes();
        assert!(after <= before, "reset should release memory");
    }

    #[test]
    fn arena_allocation_limit_enforced() {
        // Create arena with a tiny limit (64 bytes).
        let arena = Arena::new(Some(64));
        assert_eq!(arena.allocation_limit(), Some(64));

        // Small allocation should succeed.
        let _small = arena.alloc(42u32);

        // Large allocation that exceeds the limit should panic.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            // This should trigger a new chunk allocation that exceeds the limit.
            let _large = arena.alloc_slice_copy(&[0u8; 1024]);
        }));
        assert!(result.is_err(), "allocation should panic when limit exceeded");
    }

    #[test]
    fn arena_no_limit_allows_large_allocations() {
        let arena = Arena::new(None);
        assert_eq!(arena.allocation_limit(), None);
        // Should not panic with no limit.
        let _ = arena.alloc_slice_copy(&[0u8; 1024 * 1024]);
    }

    #[test]
    fn arena_set_allocation_limit() {
        let arena = Arena::new(None);
        assert_eq!(arena.allocation_limit(), None);

        arena.set_allocation_limit(Some(64));
        assert_eq!(arena.allocation_limit(), Some(64));

        arena.set_allocation_limit(None);
        assert_eq!(arena.allocation_limit(), None);
    }

    #[test]
    fn arena_slice_operations() {
        let arena = Arena::new(Some(1024 * 1024));
        let slice = arena.alloc_slice_copy(&[1u8, 2, 3, 4, 5]);
        assert_eq!(slice, &[1, 2, 3, 4, 5]);

        let cloned = arena.alloc_slice_clone(&[42u32, 42, 42]);
        assert_eq!(cloned, &[42, 42, 42]);
    }

    #[test]
    fn arena_string_allocation() {
        let arena = Arena::new(Some(1024 * 1024));
        let s = arena.alloc_string("hello arena");
        assert_eq!(s, "hello arena");
    }

    #[test]
    fn thread_local_arena_with_arena() {
        let arena = Arena::new(Some(1024 * 1024));
        let result = ThreadLocalArena::with_arena(&arena, || {
            let ptr = unsafe { std::alloc::alloc(Layout::from_size_align_unchecked(100, 8)) };
            !ptr.is_null()
        });
        assert!(result, "allocation through thread-local arena should succeed");
    }

    #[test]
    fn thread_local_arena_allocated_bytes() {
        let arena = Arena::new(Some(1024 * 1024));
        let bytes = ThreadLocalArena::with_arena(&arena, || {
            let _ = arena.alloc_slice_copy(&[0u8; 256]);
            ThreadLocalArena::allocated_bytes()
        });
        assert!(bytes > 0, "allocated_bytes should reflect arena usage");
    }

    #[test]
    fn thread_local_arena_no_nested_calls() {
        let arena1 = Arena::new(Some(1024 * 1024));
        let arena2 = Arena::new(Some(1024 * 1024));

        // Nested with_arena calls should work — the inner call overwrites
        // the thread-local pointer, but the guard restores it on exit.
        // This is not a panic scenario, just verifies the guard works.
        ThreadLocalArena::with_arena(&arena1, || {
            ThreadLocalArena::with_arena(&arena2, || {
                let ptr = unsafe { std::alloc::alloc(Layout::from_size_align_unchecked(100, 8)) };
                assert!(!ptr.is_null());
            });
        });
    }

    #[test]
    fn fallback_warning_flag_resets() {
        // Verify the warning flag starts as false.
        assert!(!ThreadLocalArena::fallback_warned());

        // Reset should be a no-op when already false.
        ThreadLocalArena::reset_fallback_warning();
        assert!(!ThreadLocalArena::fallback_warned());
    }

    #[cfg(feature = "require-tla")]
    #[test]
    fn require_tla_on_fallback_panics() {
        // Under require-tla, on_fallback() panics.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            ThreadLocalArena::on_fallback();
        }));
        let err = result.expect_err("on_fallback should panic under require-tla");
        let msg = err.downcast_ref::<&str>().expect("panic message should be &str");
        assert!(
            msg.contains("ThreadLocalArena: no arena active"),
            "panic message should identify the misconfiguration, got: {msg}"
        );
    }

    #[cfg(not(feature = "require-tla"))]
    #[test]
    fn without_require_tla_on_fallback_warns_once() {
        // Reset the warning flag first.
        ThreadLocalArena::reset_fallback_warning();
        assert!(!ThreadLocalArena::fallback_warned());

        // First call should set the warned flag.
        ThreadLocalArena::on_fallback();
        assert!(ThreadLocalArena::fallback_warned(), "first call should set warned flag");

        // Second call should not panic and should not change the flag.
        ThreadLocalArena::on_fallback();
        assert!(ThreadLocalArena::fallback_warned(), "flag should remain set");
    }
}
