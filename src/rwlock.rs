// Copyright 2016 Amanieu d'Antras
//
// Licensed under the Apache License, Version 2.0, <LICENSE-APACHE or
// http://apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. This file may not be
// copied, modified, or distributed except according to those terms.

use std::cell::UnsafeCell;
use std::ops::{Deref, DerefMut};
use std::time::{Duration, Instant};
use std::fmt;
use std::mem;
use std::marker::PhantomData;
use raw_rwlock::RawRwLock;

#[cfg(feature = "owning_ref")]
use owning_ref::StableAddress;

/// A reader-writer lock
///
/// This type of lock allows a number of readers or at most one writer at any
/// point in time. The write portion of this lock typically allows modification
/// of the underlying data (exclusive access) and the read portion of this lock
/// typically allows for read-only access (shared access).
///
/// This lock uses a task-fair locking policy which avoids both reader and
/// writer starvation. This means that readers trying to acquire the lock will
/// block even if the lock is unlocked when there are writers waiting to acquire
/// the lock. Because of this, attempts to recursively acquire a read lock
/// within a single thread may result in a deadlock.
///
/// The type parameter `T` represents the data that this lock protects. It is
/// required that `T` satisfies `Send` to be shared across threads and `Sync` to
/// allow concurrent access through readers. The RAII guards returned from the
/// locking methods implement `Deref` (and `DerefMut` for the `write` methods)
/// to allow access to the contained of the lock.
///
/// # Fairness
///
/// A typical unfair lock can often end up in a situation where a single thread
/// quickly acquires and releases the same lock in succession, which can starve
/// other threads waiting to acquire the rwlock. While this improves performance
/// because it doesn't force a context switch when a thread tries to re-acquire
/// a rwlock it has just released, this can starve other threads.
///
/// This rwlock uses [eventual fairness](https://trac.webkit.org/changeset/203350)
/// to ensure that the lock will be fair on average without sacrificing
/// performance. This is done by forcing a fair unlock on average every 0.5ms,
/// which will force the lock to go to the next thread waiting for the rwlock.
///
/// Additionally, any critical section longer than 1ms will always use a fair
/// unlock, which has a negligible performance impact compared to the length of
/// the critical section.
///
/// You can also force a fair unlock by calling `RwLockReadGuard::unlock_fair`
/// or `RwLockWriteGuard::unlock_fair` when unlocking a mutex instead of simply
/// dropping the guard.
///
/// # Differences from the standard library `RwLock`
///
/// - Supports atomically downgrading a write lock into a read lock.
/// - Task-fair locking policy instead of an unspecified platform default.
/// - No poisoning, the lock is released normally on panic.
/// - Only requires 1 word of space, whereas the standard library boxes the
///   `RwLock` due to platform limitations.
/// - Can be statically constructed (requires the `const_fn` nightly feature).
/// - Does not require any drop glue when dropped.
/// - Inline fast path for the uncontended case.
/// - Efficient handling of micro-contention using adaptive spinning.
/// - Allows raw locking & unlocking without a guard.
/// - Supports eventual fairness so that the rwlock is fair on average.
/// - Optionally allows making the rwlock fair by calling
///   `RwLockReadGuard::unlock_fair` and `RwLockWriteGuard::unlock_fair`.
///
/// # Examples
///
/// ```
/// use parking_lot::RwLock;
///
/// let lock = RwLock::new(5);
///
/// // many reader locks can be held at once
/// {
///     let r1 = lock.read();
///     let r2 = lock.read();
///     assert_eq!(*r1, 5);
///     assert_eq!(*r2, 5);
/// } // read locks are dropped at this point
///
/// // only one write lock may be held, however
/// {
///     let mut w = lock.write();
///     *w += 1;
///     assert_eq!(*w, 6);
/// } // write lock is dropped here
/// ```
pub struct RwLock<T: ?Sized> {
    raw: RawRwLock,
    data: UnsafeCell<T>,
}

unsafe impl<T: ?Sized + Send> Send for RwLock<T> {}
unsafe impl<T: ?Sized + Send + Sync> Sync for RwLock<T> {}

/// RAII structure used to release the shared read access of a lock when
/// dropped.
#[must_use]
pub struct RwLockReadGuard<'a, T: ?Sized + 'a> {
    raw: &'a RawRwLock,
    data: *const T,
    marker: PhantomData<&'a T>,
}

unsafe impl<'a, T: ?Sized + Sync + 'a> Sync for RwLockReadGuard<'a, T> {}

/// RAII structure used to release the exclusive write access of a lock when
/// dropped.
#[must_use]
pub struct RwLockWriteGuard<'a, T: ?Sized + 'a> {
    raw: &'a RawRwLock,
    data: *mut T,
    marker: PhantomData<&'a mut T>,
}

unsafe impl<'a, T: ?Sized + Sync + 'a> Sync for RwLockWriteGuard<'a, T> {}

/// RAII structure used to release the upgradable read access of a lock when
/// dropped.
#[must_use]
pub struct RwLockUpgradableReadGuard<'a, T: ?Sized + 'a> {
    raw: &'a RawRwLock,
    data: *mut T,
    marker: PhantomData<&'a T>,
}

unsafe impl<'a, T: ?Sized + Sync + 'a> Sync for RwLockUpgradableReadGuard<'a, T> {}

impl<T> RwLock<T> {
    /// Creates a new instance of an `RwLock<T>` which is unlocked.
    ///
    /// # Examples
    ///
    /// ```
    /// use parking_lot::RwLock;
    ///
    /// let lock = RwLock::new(5);
    /// ```
    #[cfg(feature = "nightly")]
    #[inline]
    pub const fn new(val: T) -> RwLock<T> {
        RwLock {
            data: UnsafeCell::new(val),
            raw: RawRwLock::new(),
        }
    }

    /// Creates a new instance of an `RwLock<T>` which is unlocked.
    ///
    /// # Examples
    ///
    /// ```
    /// use parking_lot::RwLock;
    ///
    /// let lock = RwLock::new(5);
    /// ```
    #[cfg(not(feature = "nightly"))]
    #[inline]
    pub fn new(val: T) -> RwLock<T> {
        RwLock {
            data: UnsafeCell::new(val),
            raw: RawRwLock::new(),
        }
    }

    /// Consumes this `RwLock`, returning the underlying data.
    #[inline]
    pub fn into_inner(self) -> T {
        unsafe { self.data.into_inner() }
    }
}

impl<T: ?Sized> RwLock<T> {
    #[inline]
    fn read_guard(&self) -> RwLockReadGuard<T> {
        RwLockReadGuard {
            raw: &self.raw,
            data: self.data.get(),
            marker: PhantomData,
        }
    }

    #[inline]
    fn write_guard(&self) -> RwLockWriteGuard<T> {
        RwLockWriteGuard {
            raw: &self.raw,
            data: self.data.get(),
            marker: PhantomData,
        }
    }

    #[inline]
    fn upgradable_guard(&self) -> RwLockUpgradableReadGuard<T> {
        RwLockUpgradableReadGuard {
            raw: &self.raw,
            data: self.data.get(),
            marker: PhantomData,
        }
    }

    /// Locks this rwlock with shared read access, blocking the current thread
    /// until it can be acquired.
    ///
    /// The calling thread will be blocked until there are no more writers which
    /// hold the lock. There may be other readers currently inside the lock when
    /// this method returns.
    ///
    /// Note that attempts to recursively acquire a read lock on a `RwLock` when
    /// the current thread already holds one may result in a deadlock.
    ///
    /// Returns an RAII guard which will release this thread's shared access
    /// once it is dropped.
    #[inline]
    pub fn read(&self) -> RwLockReadGuard<T> {
        self.raw.lock_shared(false);
        self.read_guard()
    }

    /// Attempts to acquire this rwlock with shared read access.
    ///
    /// If the access could not be granted at this time, then `None` is returned.
    /// Otherwise, an RAII guard is returned which will release the shared access
    /// when it is dropped.
    ///
    /// This function does not block.
    #[inline]
    pub fn try_read(&self) -> Option<RwLockReadGuard<T>> {
        if self.raw.try_lock_shared(false) {
            Some(self.read_guard())
        } else {
            None
        }
    }

    /// Attempts to acquire this rwlock with shared read access until a timeout
    /// is reached.
    ///
    /// If the access could not be granted before the timeout expires, then
    /// `None` is returned. Otherwise, an RAII guard is returned which will
    /// release the shared access when it is dropped.
    #[inline]
    pub fn try_read_for(&self, timeout: Duration) -> Option<RwLockReadGuard<T>> {
        if self.raw.try_lock_shared_for(false, timeout) {
            Some(self.read_guard())
        } else {
            None
        }
    }

    /// Attempts to acquire this rwlock with shared read access until a timeout
    /// is reached.
    ///
    /// If the access could not be granted before the timeout expires, then
    /// `None` is returned. Otherwise, an RAII guard is returned which will
    /// release the shared access when it is dropped.
    #[inline]
    pub fn try_read_until(&self, timeout: Instant) -> Option<RwLockReadGuard<T>> {
        if self.raw.try_lock_shared_until(false, timeout) {
            Some(self.read_guard())
        } else {
            None
        }
    }

    /// Locks this rwlock with shared read access, blocking the current thread
    /// until it can be acquired.
    ///
    /// The calling thread will be blocked until there are no more writers which
    /// hold the lock. There may be other readers currently inside the lock when
    /// this method returns.
    ///
    /// Unlike `read`, this method is guaranteed to succeed without blocking if
    /// another read lock is held at the time of the call. This allows a thread
    /// to recursively lock a `RwLock`. However using this method can cause
    /// writers to starve since readers no longer block if a writer is waiting
    /// for the lock.
    ///
    /// Returns an RAII guard which will release this thread's shared access
    /// once it is dropped.
    #[inline]
    pub fn read_recursive(&self) -> RwLockReadGuard<T> {
        self.raw.lock_shared(true);
        self.read_guard()
    }

    /// Attempts to acquire this rwlock with shared read access.
    ///
    /// If the access could not be granted at this time, then `None` is returned.
    /// Otherwise, an RAII guard is returned which will release the shared access
    /// when it is dropped.
    ///
    /// This method is guaranteed to succeed if another read lock is held at the
    /// time of the call. See the documentation for `read_recursive` for details.
    ///
    /// This function does not block.
    #[inline]
    pub fn try_read_recursive(&self) -> Option<RwLockReadGuard<T>> {
        if self.raw.try_lock_shared(true) {
            Some(self.read_guard())
        } else {
            None
        }
    }

    /// Attempts to acquire this rwlock with shared read access until a timeout
    /// is reached.
    ///
    /// If the access could not be granted before the timeout expires, then
    /// `None` is returned. Otherwise, an RAII guard is returned which will
    /// release the shared access when it is dropped.
    ///
    /// This method is guaranteed to succeed without blocking if another read
    /// lock is held at the time of the call. See the documentation for
    /// `read_recursive` for details.
    #[inline]
    pub fn try_read_recursive_for(&self, timeout: Duration) -> Option<RwLockReadGuard<T>> {
        if self.raw.try_lock_shared_for(true, timeout) {
            Some(self.read_guard())
        } else {
            None
        }
    }

    /// Attempts to acquire this rwlock with shared read access until a timeout
    /// is reached.
    ///
    /// If the access could not be granted before the timeout expires, then
    /// `None` is returned. Otherwise, an RAII guard is returned which will
    /// release the shared access when it is dropped.
    #[inline]
    pub fn try_read_recursive_until(&self, timeout: Instant) -> Option<RwLockReadGuard<T>> {
        if self.raw.try_lock_shared_until(true, timeout) {
            Some(self.read_guard())
        } else {
            None
        }
    }

    /// Locks this rwlock with exclusive write access, blocking the current
    /// thread until it can be acquired.
    ///
    /// This function will not return while other writers or other readers
    /// currently have access to the lock.
    ///
    /// Returns an RAII guard which will drop the write access of this rwlock
    /// when dropped.
    #[inline]
    pub fn write(&self) -> RwLockWriteGuard<T> {
        self.raw.lock_exclusive();
        self.write_guard()
    }

    /// Attempts to lock this rwlock with exclusive write access.
    ///
    /// If the lock could not be acquired at this time, then `None` is returned.
    /// Otherwise, an RAII guard is returned which will release the lock when
    /// it is dropped.
    ///
    /// This function does not block.
    #[inline]
    pub fn try_write(&self) -> Option<RwLockWriteGuard<T>> {
        if self.raw.try_lock_exclusive() {
            Some(self.write_guard())
        } else {
            None
        }
    }

    /// Attempts to acquire this rwlock with exclusive write access until a
    /// timeout is reached.
    ///
    /// If the access could not be granted before the timeout expires, then
    /// `None` is returned. Otherwise, an RAII guard is returned which will
    /// release the exclusive access when it is dropped.
    #[inline]
    pub fn try_write_for(&self, timeout: Duration) -> Option<RwLockWriteGuard<T>> {
        if self.raw.try_lock_exclusive_for(timeout) {
            Some(self.write_guard())
        } else {
            None
        }
    }

    /// Attempts to acquire this rwlock with exclusive write access until a
    /// timeout is reached.
    ///
    /// If the access could not be granted before the timeout expires, then
    /// `None` is returned. Otherwise, an RAII guard is returned which will
    /// release the exclusive access when it is dropped.
    #[inline]
    pub fn try_write_until(&self, timeout: Instant) -> Option<RwLockWriteGuard<T>> {
        if self.raw.try_lock_exclusive_until(timeout) {
            Some(self.write_guard())
        } else {
            None
        }
    }

    /// Locks this rwlock with upgradable read access, blocking the current thread
    /// until it can be acquired.
    ///
    /// The calling thread will be blocked until there are no more writers or other
    /// upgradable reads which hold the lock. There may be other readers currently
    /// inside the lock when this method returns.
    ///
    /// Returns an RAII guard which will release this thread's shared access
    /// once it is dropped.
    #[inline]
    pub fn upgradable_read(&self) -> RwLockUpgradableReadGuard<T> {
        self.raw.lock_upgradable();
        self.upgradable_guard()
    }

    /// Attempts to acquire this rwlock with upgradable read access.
    ///
    /// If the access could not be granted at this time, then `None` is returned.
    /// Otherwise, an RAII guard is returned which will release the shared access
    /// when it is dropped.
    ///
    /// This function does not block.
    #[inline]
    pub fn try_upgradable_read(&self) -> Option<RwLockUpgradableReadGuard<T>> {
        if self.raw.try_lock_upgradable() {
            Some(self.upgradable_guard())
        } else {
            None
        }
    }

    /// Attempts to acquire this rwlock with upgradable read access until a timeout
    /// is reached.
    ///
    /// If the access could not be granted before the timeout expires, then
    /// `None` is returned. Otherwise, an RAII guard is returned which will
    /// release the shared access when it is dropped.
    #[inline]
    pub fn try_upgradable_read_for(
        &self,
        timeout: Duration,
    ) -> Option<RwLockUpgradableReadGuard<T>> {
        if self.raw.try_lock_upgradable_for(timeout) {
            Some(self.upgradable_guard())
        } else {
            None
        }
    }

    /// Attempts to acquire this rwlock with upgradable read access until a timeout
    /// is reached.
    ///
    /// If the access could not be granted before the timeout expires, then
    /// `None` is returned. Otherwise, an RAII guard is returned which will
    /// release the shared access when it is dropped.
    #[inline]
    pub fn try_upgradable_read_until(
        &self,
        timeout: Instant,
    ) -> Option<RwLockUpgradableReadGuard<T>> {
        if self.raw.try_lock_upgradable_until(timeout) {
            Some(self.upgradable_guard())
        } else {
            None
        }
    }

    /// Returns a mutable reference to the underlying data.
    ///
    /// Since this call borrows the `RwLock` mutably, no actual locking needs to
    /// take place---the mutable borrow statically guarantees no locks exist.
    #[inline]
    pub fn get_mut(&mut self) -> &mut T {
        unsafe { &mut *self.data.get() }
    }

    /// Releases shared read access of the rwlock.
    ///
    /// # Safety
    ///
    /// This function must only be called if the rwlock was locked using
    /// `raw_read` or `raw_try_read`, or if an `RwLockReadGuard` from this
    /// rwlock was leaked (e.g. with `mem::forget`). The rwlock must be locked
    /// with shared read access.
    #[inline]
    pub unsafe fn raw_unlock_read(&self) {
        self.raw.unlock_shared(false);
    }

    /// Releases exclusive write access of the rwlock.
    ///
    /// # Safety
    ///
    /// This function must only be called if the rwlock was locked using
    /// `raw_write` or `raw_try_write`, or if an `RwLockWriteGuard` from this
    /// rwlock was leaked (e.g. with `mem::forget`). The rwlock must be locked
    /// with exclusive write access.
    #[inline]
    pub unsafe fn raw_unlock_write(&self) {
        self.raw.unlock_exclusive(false);
    }

    /// Releases upgradable read access of the rwlock.
    ///
    /// # Safety
    ///
    /// This function must only be called if the rwlock was locked using
    /// `raw_upgradable_read` or `raw_try_upgradable_read`, or if an
    /// `RwLockUpgradableReadGuard` from this rwlock was leaked (e.g. with
    /// `mem::forget`). The rwlock must be locked with upgradable read access.
    #[inline]
    pub unsafe fn raw_unlock_upgradable_read(&self) {
        self.raw.unlock_upgradable(false);
    }

    /// Releases shared read access of the rwlock using a fair unlock protocol.
    ///
    /// See `RwLockReadGuard::unlock_fair`.
    ///
    /// # Safety
    ///
    /// This function must only be called if the rwlock was locked using
    /// `raw_write` or `raw_try_write`, a raw upgradable read lock was upgraded
    /// using `raw_upgrade` or `raw_try_upgrade`, or if an `RwLockWriteGuard`
    /// from this rwlock was leaked (e.g. with `mem::forget`). The rwlock must
    /// be locked with exclusive write access.
    #[inline]
    pub unsafe fn raw_unlock_read_fair(&self) {
        self.raw.unlock_shared(true);
    }

    /// Releases exclusive write access of the rwlock using a fair unlock
    /// protocol.
    ///
    /// See `RwLockWriteGuard::unlock_fair`.
    ///
    /// # Safety
    ///
    /// This function must only be called if the rwlock was locked using
    /// `raw_write` or `raw_try_write`, a raw upgradable read lock was upgraded
    /// using `raw_upgrade` or `raw_try_upgrade`, or if an `RwLockWriteGuard`
    /// from this rwlock was leaked (e.g. with `mem::forget`). The rwlock must
    /// be locked with exclusive write access.
    #[inline]
    pub unsafe fn raw_unlock_write_fair(&self) {
        self.raw.unlock_exclusive(true);
    }

    /// Releases upgradable read access of the rwlock using a fair unlock
    /// protocol.
    ///
    /// # Safety
    ///
    /// This function must only be called if the rwlock was locked using
    /// `raw_upgradable_read` or `raw_try_upgradable_read`, or if an
    /// `RwLockUpgradableReadGuard` from this rwlock was leaked (e.g. with
    /// `mem::forget`). The rwlock must be locked with upgradable read access.
    #[inline]
    pub unsafe fn raw_unlock_upgradable_read_fair(&self) {
        self.raw.unlock_upgradable(true);
    }

    /// Atomically downgrades a write lock into a shared read lock without
    /// allowing any writers to take exclusive access of the lock in the meantime.
    ///
    /// See `RwLockWriteGuard::downgrade`.
    ///
    /// # Safety
    ///
    /// This function must only be called if the rwlock was locked using
    /// `raw_write` or `raw_try_write`, or if an `RwLockWriteGuard` from this
    /// rwlock was leaked (e.g. with `mem::forget`). The rwlock must be locked
    /// with exclusive write access.
    #[inline]
    pub unsafe fn raw_downgrade(&self) {
        self.raw.exclusive_to_shared();
    }

    /// Atomically downgrades an upgradable read lock into a shared read lock
    /// without allowing any writers to take exclusive access of the lock in
    /// the meantime.
    ///
    /// See `RwLockUpgradableReadGuard::downgrade`.
    ///
    /// # Safety
    ///
    /// This function must only be called if the rwlock was locked using
    /// `raw_upgradable_read` or `raw_try_upgradable_read`, or if an
    /// `RwLockUpgradableReadGuard` from this rwlock was leaked (e.g. with
    /// `mem::forget`). The rwlock must be locked with upgradable read access.
    #[inline]
    pub unsafe fn raw_downgrade_upgradable_read(&self) {
        self.raw.upgradable_to_shared();
    }
}

impl RwLock<()> {
    /// Locks this rwlock with shared read access, blocking the current thread
    /// until it can be acquired.
    ///
    /// This is similar to `read`, except that a `RwLockReadGuard` is not
    /// returned. Instead you will need to call `raw_unlock` to release the
    /// rwlock.
    #[inline]
    pub fn raw_read(&self) {
        self.raw.lock_shared(false);
    }

    /// Attempts to acquire this rwlock with shared read access.
    ///
    /// This is similar to `try_read`, except that a `RwLockReadGuard` is not
    /// returned. Instead you will need to call `raw_unlock` to release the
    /// rwlock.
    #[inline]
    pub fn raw_try_read(&self) -> bool {
        self.raw.try_lock_shared(false)
    }

    /// Locks this rwlock with shared read access, blocking the current thread
    /// until it can be acquired.
    ///
    /// This is similar to `read_recursive`, except that a `RwLockReadGuard` is
    /// not returned. Instead you will need to call `raw_unlock` to release the
    /// rwlock.
    #[inline]
    pub fn raw_read_recursive(&self) {
        self.raw.lock_shared(true);
    }

    /// Attempts to acquire this rwlock with shared read access.
    ///
    /// This is similar to `try_read_recursive`, except that a `RwLockReadGuard` is not
    /// returned. Instead you will need to call `raw_unlock` to release the
    /// rwlock.
    #[inline]
    pub fn raw_try_read_recursive(&self) -> bool {
        self.raw.try_lock_shared(true)
    }

    /// Locks this rwlock with exclusive write access, blocking the current
    /// thread until it can be acquired.
    ///
    /// This is similar to `write`, except that a `RwLockReadGuard` is not
    /// returned. Instead you will need to call `raw_unlock` to release the
    /// rwlock.
    #[inline]
    pub fn raw_write(&self) {
        self.raw.lock_exclusive();
    }

    /// Attempts to lock this rwlock with exclusive write access.
    ///
    /// This is similar to `try_write`, except that a `RwLockReadGuard` is not
    /// returned. Instead you will need to call `raw_unlock` to release the
    /// rwlock.
    #[inline]
    pub fn raw_try_write(&self) -> bool {
        self.raw.try_lock_exclusive()
    }

    /// Locks this rwlock with upgradable read access, blocking the current thread
    /// until it can be acquired.
    ///
    /// This is similar to `upgradable_read`, except that a
    /// `RwLockUpgradableReadGuard` is not returned. Instead you will need to call
    /// `raw_unlock` to release the rwlock.
    #[inline]
    pub fn raw_upgradable_read(&self) {
        self.raw.lock_upgradable();
    }

    /// Attempts to acquire this rwlock with upgradable read access.
    ///
    /// This is similar to `try_upgradable_read`, except that a
    /// `RwLockUpgradableReadGuard` is not returned. Instead you will need to call
    /// `raw_unlock` to release the rwlock.
    #[inline]
    pub fn raw_try_upgradable_read(&self) -> bool {
        self.raw.try_lock_upgradable()
    }

    /// Upgrades this rwlock from upgradable read access to exclusive write access,
    /// blocking the current thread until it can be acquired.
    ///
    /// See `RwLockUpgradableReadGuard::upgrade`.
    ///
    /// # Safety
    ///
    /// This function must only be called if the rwlock was locked using
    /// `raw_upgradable_read` or `raw_try_upgradable_read`, or if an
    /// `RwLockUpgradableReadGuard` from this rwlock was leaked (e.g. with
    /// `mem::forget`). The rwlock must be locked with upgradable read access.
    #[inline]
    pub unsafe fn raw_upgrade(&self) {
        self.raw.upgradable_to_exclusive();
    }

    /// Attempts to upgrade this rwlock from upgradable read access to exclusive
    /// write access.
    ///
    /// See `RwLockUpgradableReadGuard::try_upgrade`.
    ///
    /// # Safety
    ///
    /// This function must only be called if the rwlock was locked using
    /// `raw_upgradable_read` or `raw_try_upgradable_read`, or if an
    /// `RwLockUpgradableReadGuard` from this rwlock was leaked (e.g. with
    /// `mem::forget`). The rwlock must be locked with upgradable read access.
    #[inline]
    pub unsafe fn raw_try_upgrade(&self) -> bool {
        self.raw.try_upgradable_to_exclusive()
    }
}

impl<T: ?Sized + Default> Default for RwLock<T> {
    #[inline]
    fn default() -> RwLock<T> {
        RwLock::new(Default::default())
    }
}

impl<T: ?Sized + fmt::Debug> fmt::Debug for RwLock<T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self.try_read() {
            Some(guard) => f.debug_struct("RwLock")
                .field("data", &&*guard)
                .finish(),
            None => f.pad("RwLock { <locked> }"),
        }
    }
}

impl<'a, T: ?Sized + 'a> RwLockReadGuard<'a, T> {
    /// Unlocks the `RwLock` using a fair unlock protocol.
    ///
    /// By default, `RwLock` is unfair and allow the current thread to re-lock
    /// the rwlock before another has the chance to acquire the lock, even if
    /// that thread has been blocked on the `RwLock` for a long time. This is
    /// the default because it allows much higher throughput as it avoids
    /// forcing a context switch on every rwlock unlock. This can result in one
    /// thread acquiring a `RwLock` many more times than other threads.
    ///
    /// However in some cases it can be beneficial to ensure fairness by forcing
    /// the lock to pass on to a waiting thread if there is one. This is done by
    /// using this method instead of dropping the `RwLockReadGuard` normally.
    #[inline]
    pub fn unlock_fair(self) {
        self.raw.unlock_shared(true);
        mem::forget(self);
    }

    /// Make a new `RwLockReadGuard` for a component of the locked data.
    ///
    /// This operation cannot fail as the `RwLockReadGuard` passed
    /// in already locked the data.
    ///
    /// This is an associated function that needs to be
    /// used as `RwLockReadGuard::map(...)`. A method would interfere with methods of
    /// the same name on the contents of the locked data.
    #[inline]
    pub fn map<U: ?Sized, F>(orig: Self, f: F) -> RwLockReadGuard<'a, U>
    where
        F: FnOnce(&T) -> &U,
    {
        let raw = orig.raw;
        let data = f(unsafe { &*orig.data });
        mem::forget(orig);
        RwLockReadGuard {
            raw,
            data,
            marker: PhantomData,
        }
    }
}

impl<'a, T: ?Sized + 'a> Deref for RwLockReadGuard<'a, T> {
    type Target = T;
    #[inline]
    fn deref(&self) -> &T {
        unsafe { &*self.data }
    }
}

impl<'a, T: ?Sized + 'a> Drop for RwLockReadGuard<'a, T> {
    #[inline]
    fn drop(&mut self) {
        self.raw.unlock_shared(false);
    }
}

#[cfg(feature = "owning_ref")]
unsafe impl<'a, T: ?Sized> StableAddress for RwLockReadGuard<'a, T> {}

impl<'a, T: ?Sized + 'a> RwLockWriteGuard<'a, T> {
    /// Atomically downgrades a write lock into a read lock without allowing any
    /// writers to take exclusive access of the lock in the meantime.
    ///
    /// Note that if there are any writers currently waiting to take the lock
    /// then other readers may not be able to acquire the lock even if it was
    /// downgraded.
    pub fn downgrade(self) -> RwLockReadGuard<'a, T> {
        self.raw.exclusive_to_shared();
        let raw = self.raw;
        // Reborrow the value to avoid moving self.borrow,
        // which isn't allow for types with destructors
        let data = unsafe { &*self.data };
        mem::forget(self);
        RwLockReadGuard {
            raw,
            data,
            marker: PhantomData,
        }
    }

    /// Make a new `RwLockWriteGuard` for a component of the locked data.
    ///
    /// This operation cannot fail as the `RwLockWriteGuard` passed
    /// in already locked the data.
    ///
    /// This is an associated function that needs to be
    /// used as `RwLockWriteGuard::map(...)`. A method would interfere with methods of
    /// the same name on the contents of the locked data.
    #[inline]
    pub fn map<U: ?Sized, F>(orig: Self, f: F) -> RwLockWriteGuard<'a, U>
    where
        F: FnOnce(&mut T) -> &mut U,
    {
        let raw = orig.raw;
        let data = f(unsafe { &mut *orig.data });
        mem::forget(orig);
        RwLockWriteGuard {
            raw,
            data,
            marker: PhantomData,
        }
    }

    /// Unlocks the `RwLock` using a fair unlock protocol.
    ///
    /// By default, `RwLock` is unfair and allow the current thread to re-lock
    /// the rwlock before another has the chance to acquire the lock, even if
    /// that thread has been blocked on the `RwLock` for a long time. This is
    /// the default because it allows much higher throughput as it avoids
    /// forcing a context switch on every rwlock unlock. This can result in one
    /// thread acquiring a `RwLock` many more times than other threads.
    ///
    /// However in some cases it can be beneficial to ensure fairness by forcing
    /// the lock to pass on to a waiting thread if there is one. This is done by
    /// using this method instead of dropping the `RwLockWriteGuard` normally.
    #[inline]
    pub fn unlock_fair(self) {
        self.raw.unlock_exclusive(true);
        mem::forget(self);
    }
}

impl<'a, T: ?Sized + 'a> Deref for RwLockWriteGuard<'a, T> {
    type Target = T;
    #[inline]
    fn deref(&self) -> &T {
        unsafe { &*self.data }
    }
}

impl<'a, T: ?Sized + 'a> DerefMut for RwLockWriteGuard<'a, T> {
    #[inline]
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.data }
    }
}

impl<'a, T: ?Sized + 'a> Drop for RwLockWriteGuard<'a, T> {
    #[inline]
    fn drop(&mut self) {
        self.raw.unlock_exclusive(false);
    }
}

#[cfg(feature = "owning_ref")]
unsafe impl<'a, T: ?Sized> StableAddress for RwLockWriteGuard<'a, T> {}

impl<'a, T: ?Sized + 'a> RwLockUpgradableReadGuard<'a, T> {
    /// Atomically downgrades an upgradable read lock lock into a shared read lock
    /// without allowing any writers to take exclusive access of the lock in the
    /// meantime.
    ///
    /// Note that if there are any writers currently waiting to take the lock
    /// then other readers may not be able to acquire the lock even if it was
    /// downgraded.
    pub fn downgrade(self) -> RwLockReadGuard<'a, T> {
        self.raw.upgradable_to_shared();
        let raw = self.raw;
        // Reborrow the value to avoid moving self.borrow,
        // which isn't allow for types with destructors
        let data = unsafe { &*self.data };
        mem::forget(self);
        RwLockReadGuard {
            raw,
            data,
            marker: PhantomData,
        }
    }

    /// Atomically upgrades an upgradable read lock lock into a exclusive write lock,
    /// blocking the current thread until it can be aquired.
    pub fn upgrade(self) -> RwLockWriteGuard<'a, T> {
        self.raw.upgradable_to_exclusive();
        let raw = self.raw;
        // Reborrow the value to avoid moving self.borrow,
        // which isn't allow for types with destructors
        let data = unsafe { &mut *self.data };
        mem::forget(self);
        RwLockWriteGuard {
            raw,
            data,
            marker: PhantomData,
        }
    }

    /// Tries to atomically upgrade an upgradable read lock into a exclusive write lock.
    ///
    /// If the access could not be granted at this time, then the current guard is returned.
    pub fn try_upgrade(self) -> Result<RwLockWriteGuard<'a, T>, Self> {
        if self.raw.try_upgradable_to_exclusive() {
            let raw = self.raw;
            // Reborrow the value to avoid moving self.borrow,
            // which isn't allow for types with destructors
            let data = unsafe { &mut *self.data };
            mem::forget(self);
            Ok(RwLockWriteGuard {
                raw,
                data,
                marker: PhantomData,
            })
        } else {
            Err(self)
        }
    }

    /// Tries to atomically upgrade an upgradable read lock into a exclusive
    /// write lock, until a timeout is reached.
    ///
    /// If the access could not be granted before the timeout expires, then
    /// the current guard is returned.
    pub fn try_upgrade_for(self, timeout: Duration) -> Result<RwLockWriteGuard<'a, T>, Self> {
        if self.raw.try_upgradable_to_exclusive_for(timeout) {
            let raw = self.raw;
            // Reborrow the value to avoid moving self.borrow,
            // which isn't allow for types with destructors
            let data = unsafe { &mut *self.data };
            mem::forget(self);
            Ok(RwLockWriteGuard {
                raw,
                data,
                marker: PhantomData,
            })
        } else {
            Err(self)
        }
    }

    /// Tries to atomically upgrade an upgradable read lock into a exclusive
    /// write lock, until a timeout is reached.
    ///
    /// If the access could not be granted before the timeout expires, then
    /// the current guard is returned.
    #[inline]
    pub fn try_upgrade_until(self, timeout: Instant) -> Result<RwLockWriteGuard<'a, T>, Self> {
        if self.raw.try_upgradable_to_exclusive_until(timeout) {
            let raw = self.raw;
            // Reborrow the value to avoid moving self.borrow,
            // which isn't allow for types with destructors
            let data = unsafe { &mut *self.data };
            mem::forget(self);
            Ok(RwLockWriteGuard {
                raw,
                data,
                marker: PhantomData,
            })
        } else {
            Err(self)
        }
    }

    /// Unlocks the `RwLock` using a fair unlock protocol.
    ///
    /// By default, `RwLock` is unfair and allow the current thread to re-lock
    /// the rwlock before another has the chance to acquire the lock, even if
    /// that thread has been blocked on the `RwLock` for a long time. This is
    /// the default because it allows much higher throughput as it avoids
    /// forcing a context switch on every rwlock unlock. This can result in one
    /// thread acquiring a `RwLock` many more times than other threads.
    ///
    /// However in some cases it can be beneficial to ensure fairness by forcing
    /// the lock to pass on to a waiting thread if there is one. This is done by
    /// using this method instead of dropping the `RwLockUpgradableReadGuard` normally.
    #[inline]
    pub fn unlock_fair(self) {
        self.raw.unlock_upgradable(true);
        mem::forget(self);
    }
}

impl<'a, T: ?Sized + 'a> Deref for RwLockUpgradableReadGuard<'a, T> {
    type Target = T;
    #[inline]
    fn deref(&self) -> &T {
        unsafe { &*self.data }
    }
}

impl<'a, T: ?Sized + 'a> Drop for RwLockUpgradableReadGuard<'a, T> {
    #[inline]
    fn drop(&mut self) {
        self.raw.unlock_upgradable(false);
    }
}

#[cfg(feature = "owning_ref")]
unsafe impl<'a, T: ?Sized> StableAddress for RwLockUpgradableReadGuard<'a, T> {}

#[cfg(test)]
mod tests {
    extern crate rand;
    use self::rand::Rng;
    use std::sync::mpsc::channel;
    use std::thread;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    use RwLock;

    #[derive(Eq, PartialEq, Debug)]
    struct NonCopy(i32);

    #[test]
    fn smoke() {
        let l = RwLock::new(());
        drop(l.read());
        drop(l.write());
        drop(l.upgradable_read());
        drop((l.read(), l.read()));
        drop((l.read(), l.upgradable_read()));
        drop(l.write());
    }

    #[test]
    fn frob() {
        const N: u32 = 10;
        const M: u32 = 1000;

        let r = Arc::new(RwLock::new(()));

        let (tx, rx) = channel::<()>();
        for _ in 0..N {
            let tx = tx.clone();
            let r = r.clone();
            thread::spawn(move || {
                let mut rng = rand::thread_rng();
                for _ in 0..M {
                    if rng.gen_bool(1.0 / N as f64) {
                        drop(r.write());
                    } else {
                        drop(r.read());
                    }
                }
                drop(tx);
            });
        }
        drop(tx);
        let _ = rx.recv();
    }

    #[test]
    fn test_rw_arc_no_poison_wr() {
        let arc = Arc::new(RwLock::new(1));
        let arc2 = arc.clone();
        let _: Result<(), _> = thread::spawn(move || {
            let _lock = arc2.write();
            panic!();
        }).join();
        let lock = arc.read();
        assert_eq!(*lock, 1);
    }

    #[test]
    fn test_rw_arc_no_poison_ww() {
        let arc = Arc::new(RwLock::new(1));
        let arc2 = arc.clone();
        let _: Result<(), _> = thread::spawn(move || {
            let _lock = arc2.write();
            panic!();
        }).join();
        let lock = arc.write();
        assert_eq!(*lock, 1);
    }

    #[test]
    fn test_rw_arc_no_poison_rr() {
        let arc = Arc::new(RwLock::new(1));
        let arc2 = arc.clone();
        let _: Result<(), _> = thread::spawn(move || {
            let _lock = arc2.read();
            panic!();
        }).join();
        let lock = arc.read();
        assert_eq!(*lock, 1);
    }

    #[test]
    fn test_rw_arc_no_poison_rw() {
        let arc = Arc::new(RwLock::new(1));
        let arc2 = arc.clone();
        let _: Result<(), _> = thread::spawn(move || {
            let _lock = arc2.read();
            panic!()
        }).join();
        let lock = arc.write();
        assert_eq!(*lock, 1);
    }

    #[test]
    fn test_ruw_arc() {
        let arc = Arc::new(RwLock::new(0));
        let arc2 = arc.clone();
        let (tx, rx) = channel();

        thread::spawn(move || {
            for _ in 0..10 {
                let mut lock = arc2.write();
                let tmp = *lock;
                *lock = -1;
                thread::yield_now();
                *lock = tmp + 1;
            }
            tx.send(()).unwrap();
        });

        let mut children = Vec::new();

        // Upgradable readers try to catch the writer in the act and also
        // try to touch the value
        for _ in 0..5 {
            let arc3 = arc.clone();
            children.push(thread::spawn(move || {
                let lock = arc3.upgradable_read();
                let tmp = *lock;
                assert!(tmp >= 0);
                thread::yield_now();
                let mut lock = lock.upgrade();
                assert_eq!(tmp, *lock);
                *lock = -1;
                thread::yield_now();
                *lock = tmp + 1;
            }));
        }

        // Readers try to catch the writers in the act
        for _ in 0..5 {
            let arc4 = arc.clone();
            children.push(thread::spawn(move || {
                let lock = arc4.read();
                assert!(*lock >= 0);
            }));
        }

        // Wait for children to pass their asserts
        for r in children {
            assert!(r.join().is_ok());
        }

        // Wait for writer to finish
        rx.recv().unwrap();
        let lock = arc.read();
        assert_eq!(*lock, 15);
    }

    #[test]
    fn test_rw_arc() {
        let arc = Arc::new(RwLock::new(0));
        let arc2 = arc.clone();
        let (tx, rx) = channel();

        thread::spawn(move || {
            let mut lock = arc2.write();
            for _ in 0..10 {
                let tmp = *lock;
                *lock = -1;
                thread::yield_now();
                *lock = tmp + 1;
            }
            tx.send(()).unwrap();
        });

        // Readers try to catch the writer in the act
        let mut children = Vec::new();
        for _ in 0..5 {
            let arc3 = arc.clone();
            children.push(thread::spawn(move || {
                let lock = arc3.read();
                assert!(*lock >= 0);
            }));
        }

        // Wait for children to pass their asserts
        for r in children {
            assert!(r.join().is_ok());
        }

        // Wait for writer to finish
        rx.recv().unwrap();
        let lock = arc.read();
        assert_eq!(*lock, 10);
    }

    #[test]
    fn test_rw_arc_access_in_unwind() {
        let arc = Arc::new(RwLock::new(1));
        let arc2 = arc.clone();
        let _ = thread::spawn(move || -> () {
            struct Unwinder {
                i: Arc<RwLock<isize>>,
            }
            impl Drop for Unwinder {
                fn drop(&mut self) {
                    let mut lock = self.i.write();
                    *lock += 1;
                }
            }
            let _u = Unwinder { i: arc2 };
            panic!();
        }).join();
        let lock = arc.read();
        assert_eq!(*lock, 2);
    }

    #[test]
    fn test_rwlock_unsized() {
        let rw: &RwLock<[i32]> = &RwLock::new([1, 2, 3]);
        {
            let b = &mut *rw.write();
            b[0] = 4;
            b[2] = 5;
        }
        let comp: &[i32] = &[4, 2, 5];
        assert_eq!(&*rw.read(), comp);
    }

    #[test]
    fn test_rwlock_try_read() {
        let lock = RwLock::new(0isize);
        {
            let read_guard = lock.read();

            let read_result = lock.try_read();
            assert!(
                read_result.is_some(),
                "try_read should succeed while read_guard is in scope"
            );

            drop(read_guard);
        }
        {
            let upgrade_guard = lock.upgradable_read();

            let read_result = lock.try_read();
            assert!(
                read_result.is_some(),
                "try_read should succeed while upgrade_guard is in scope"
            );

            drop(upgrade_guard);
        }
        {
            let write_guard = lock.write();

            let read_result = lock.try_read();
            assert!(
                read_result.is_none(),
                "try_read should fail while write_guard is in scope"
            );

            drop(write_guard);
        }
    }

    #[test]
    fn test_rwlock_try_write() {
        let lock = RwLock::new(0isize);
        {
            let read_guard = lock.read();

            let write_result = lock.try_write();
            assert!(
                write_result.is_none(),
                "try_write should fail while read_guard is in scope"
            );

            drop(read_guard);
        }
        {
            let upgrade_guard = lock.upgradable_read();

            let write_result = lock.try_write();
            assert!(
                write_result.is_none(),
                "try_write should fail while upgrade_guard is in scope"
            );

            drop(upgrade_guard);
        }
        {
            let write_guard = lock.write();

            let write_result = lock.try_write();
            assert!(
                write_result.is_none(),
                "try_write should fail while write_guard is in scope"
            );

            drop(write_guard);
        }
    }

    #[test]
    fn test_rwlock_try_upgrade() {
        let lock = RwLock::new(0isize);
        {
            let read_guard = lock.read();

            let upgrade_result = lock.try_upgradable_read();
            assert!(
                upgrade_result.is_some(),
                "try_upgradable_read should succeed while read_guard is in scope"
            );

            drop(read_guard);
        }
        {
            let upgrade_guard = lock.upgradable_read();

            let upgrade_result = lock.try_upgradable_read();
            assert!(
                upgrade_result.is_none(),
                "try_upgradable_read should fail while upgrade_guard is in scope"
            );

            drop(upgrade_guard);
        }
        {
            let write_guard = lock.write();

            let upgrade_result = lock.try_upgradable_read();
            assert!(
                upgrade_result.is_none(),
                "try_upgradable should fail while write_guard is in scope"
            );

            drop(write_guard);
        }
    }

    #[test]
    fn test_into_inner() {
        let m = RwLock::new(NonCopy(10));
        assert_eq!(m.into_inner(), NonCopy(10));
    }

    #[test]
    fn test_into_inner_drop() {
        struct Foo(Arc<AtomicUsize>);
        impl Drop for Foo {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        let num_drops = Arc::new(AtomicUsize::new(0));
        let m = RwLock::new(Foo(num_drops.clone()));
        assert_eq!(num_drops.load(Ordering::SeqCst), 0);
        {
            let _inner = m.into_inner();
            assert_eq!(num_drops.load(Ordering::SeqCst), 0);
        }
        assert_eq!(num_drops.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn test_get_mut() {
        let mut m = RwLock::new(NonCopy(10));
        *m.get_mut() = NonCopy(20);
        assert_eq!(m.into_inner(), NonCopy(20));
    }

    #[test]
    fn test_rwlockguard_sync() {
        fn sync<T: Sync>(_: T) {}

        let rwlock = RwLock::new(());
        sync(rwlock.read());
        sync(rwlock.write());
    }

    #[test]
    fn test_rwlock_downgrade() {
        let x = Arc::new(RwLock::new(0));
        let mut handles = Vec::new();
        for _ in 0..8 {
            let x = x.clone();
            handles.push(thread::spawn(move || {
                for _ in 0..100 {
                    let mut writer = x.write();
                    *writer += 1;
                    let cur_val = *writer;
                    let reader = writer.downgrade();
                    assert_eq!(cur_val, *reader);
                }
            }));
        }
        for handle in handles {
            handle.join().unwrap()
        }
        assert_eq!(*x.read(), 800);
    }

    #[test]
    fn test_rwlock_recursive() {
        let arc = Arc::new(RwLock::new(1));
        let arc2 = arc.clone();
        let _lock1 = arc.read();
        thread::spawn(move || {
            let _lock = arc2.write();
        });
        thread::sleep(Duration::from_millis(100));

        // A normal read would block here since there is a pending writer
        let _lock2 = arc.read_recursive();
    }

    #[test]
    fn test_rwlock_debug() {
        let x = RwLock::new(vec![0u8, 10]);

        assert_eq!(format!("{:?}", x), "RwLock { data: [0, 10] }");
        assert_eq!(format!("{:#?}", x),
"RwLock {
    data: [
        0,
        10
    ]
}"
        );
        let _lock = x.write();
        assert_eq!(format!("{:?}", x), "RwLock { <locked> }");
    }
}
