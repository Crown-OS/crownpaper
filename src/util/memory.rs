/// Keeps every thread on one malloc arena. The worker threads allocate only
/// in short bursts, and per-thread arenas would otherwise keep their peak
/// pages resident long after the daemon is idle again.
pub fn use_single_arena() {
    // SAFETY: mallopt only adjusts allocator tunables; called before any
    // worker thread exists.
    unsafe {
        libc::mallopt(libc::M_ARENA_MAX, 1);
    }
}

/// Hands freed heap pages back to the kernel. glibc keeps them mapped after
/// large frees, which would leave a torn-down renderer's memory in our RSS.
pub fn trim_heap() {
    // SAFETY: malloc_trim only walks allocator metadata; it has no
    // preconditions and is thread-safe.
    unsafe {
        libc::malloc_trim(0);
    }
}
