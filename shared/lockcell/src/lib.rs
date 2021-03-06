//! Inner-mutability on shared variables through spinlocks

#![no_std]
#![feature(const_fn, track_caller)]

use core::ops::{Deref, DerefMut};
use core::cell::UnsafeCell;
use core::marker::PhantomData;
use core::sync::atomic::{AtomicU32, Ordering, spin_loop_hint};

/// Trait that allows access to OS-level constructs defining interrupt state,
/// exception state, unique core IDs, and enter/exit lock (for interrupt
/// disabling and enabling) primitives.
pub trait InterruptState {
    /// Returns `true` if we're currently in an interrupt
    fn in_interrupt() -> bool;
    
    /// Returns `true` if we're currently in an exception. Which indicates that
    /// a lock cannot be held as we may have pre-empted a non-preemptable lock
    fn in_exception() -> bool;

    /// Gets the ID of the running core. It's required that this core ID is
    /// unique to the core, and cannot be `!0`
    fn core_id() -> u32;

    /// A lock which does not allow interrupting was taken, and thus interrupts
    /// must be disabled. It's up to the callee to handle the nesting of the
    /// interrupt status. Eg. using a refcount of number of interrupt disable
    /// requests
    fn enter_lock();

    /// A lock which does not allow interrupting was released, and thus
    /// interrupts can be enabled. It's up to the callee to handle the nesting
    /// of the interrupt status. Eg. using a refcount of number of interrupt
    /// disable requests
    fn exit_lock();
}

/// A spinlock-guarded variable
#[repr(C)]
pub struct LockCell<T: ?Sized, I: InterruptState> {
    /// A ticket for the lock. You grab this ticket and then wait until
    /// `release` is set to your ticket
    ticket: AtomicU32,

    /// Tracks which ticket currently owns the lock
    release: AtomicU32,

    /// Tracks the core that currently holds the lock
    owner: AtomicU32,

    /// A holder of the `InterruptState` trait for this implementation
    _interrupt_state: PhantomData<I>,

    /// If set to `true`, it is required that interrupts are disabled prior to
    /// this lock being taken.
    disables_interrupts: bool,
    
    /// Value which is guarded by locks
    val: UnsafeCell<T>,
}
unsafe impl<T: ?Sized, I: InterruptState> Sync for LockCell<T, I> {}

impl<T, I: InterruptState> LockCell<T, I> {
    /// Move a `val` into a `LockCell`, a type which allows inner mutability
    /// around ticket spinlocks.
    pub const fn new(val: T) -> Self {
        LockCell {
            ticket:              AtomicU32::new(0),
            release:             AtomicU32::new(0),
            owner:               AtomicU32::new(0),
            val:                 UnsafeCell::new(val),
            disables_interrupts: false,
            _interrupt_state:    PhantomData,
        }
    }

    /// Create a new `LockCell` which will disable interrupts for the entire
    /// time the lock is held.
    pub const fn new_no_preempt(val: T) -> Self {
        LockCell {
            ticket:              AtomicU32::new(0),
            release:             AtomicU32::new(0),
            owner:               AtomicU32::new(0),
            val:                 UnsafeCell::new(val),
            disables_interrupts: true,
            _interrupt_state:    PhantomData,
        }
    }
}

impl<T: ?Sized, I: InterruptState> LockCell<T, I> {
    /// Attempt to get exclusive access to the contained value. If `try_lock`
    /// is set to `true`, the lock is only attempted once and if it fails
    /// a `None` is returned. If `try_lock` is set to `false`, this will block
    /// until the lock is obtained.
    #[track_caller]
    fn lock_int(&self, try_lock: bool) -> Option<LockCellGuard<T, I>> {
        // If this lock does not disable interrupts, and we're currently in
        // an interrupt. Then, we just used a non-preemptable lock during an
        // interrupt. This means the lock creation for this lock should be
        // changed to a `new_no_preempt`
        assert!(self.disables_interrupts || !I::in_interrupt(),
            "Attempted to take a non-preemptable lock in an interrupt");

        // Make sure that there are no uses of blocking locks in exception
        // handlers.
        assert!(try_lock || !I::in_exception(),
            "Attempted to take a blocking lock while in an exception");
        
        // Get the core ID of the running core
        let core_id = I::core_id();

        // Disable interrupts if needed
        if self.disables_interrupts {
            I::enter_lock();
        }

        if try_lock {
            // Try locks are special, we need to guarantee we will succeed in
            // taking the lock.

            // Get the number of the ticket that is ready right now
            let current_release = self.release.load(Ordering::SeqCst);
            
            // Attempt to take the winning ticket. If we cannot get the
            // winning ticket, then give up.
            if self.ticket.compare_and_swap(
                    current_release, current_release.wrapping_add(1),
                    Ordering::SeqCst) != current_release {
                // We didn't win the lock, thus return early
                if self.disables_interrupts {
                    I::exit_lock();
                }

                return None;
            }
        } else {
            // Take a ticket
            let ticket = self.ticket.fetch_add(1, Ordering::SeqCst);
            while self.release.load(Ordering::SeqCst) != ticket {
                // If the current core is the owner of the load
                if self.owner.load(Ordering::SeqCst) == core_id {
                    panic!("Deadlock detected");
                }

                spin_loop_hint();
            }
        }

        // Note that this core owns the lock
        self.owner.store(core_id, Ordering::SeqCst);

        // At this point we have exclusive access
        Some(LockCellGuard {
            cell: self,
        })
    }

    /// Get exclusive access to the value guarded by the lock
    #[track_caller]
    pub fn lock(&self) -> LockCellGuard<T, I> {
        self.lock_int(false).unwrap()
    }
    
    /// Get exclusive access to the value guarded by the lock, if the lock
    /// is already held, returns `None`
    #[track_caller]
    pub fn try_lock(&self) -> Option<LockCellGuard<T, I>> {
        self.lock_int(true)
    }

    /// Return a raw pointer to the internal locked value, regardless of the
    /// lock state. This bypasses the lock.
    pub unsafe fn shatter(&self) -> *mut T {
        self.val.get()
    }
}

/// A guard structure which can implement `Drop` such that locks can be
/// automatically released based on scope.
pub struct LockCellGuard<'a, T: ?Sized, I: InterruptState> {
    /// A reference to the value we currently have exclusive access to
    cell: &'a LockCell<T, I>,
}

impl<'a, T: ?Sized, I: InterruptState> Drop for LockCellGuard<'a, T, I> {
    fn drop(&mut self) {
        // Set that there is no owner of the lock
        self.cell.owner.store(!0, Ordering::SeqCst);

        // Release the lock
        self.cell.release.fetch_add(1, Ordering::SeqCst);
        
        // Enable interrupts if needed
        if self.cell.disables_interrupts {
            I::exit_lock();
        }
    }
}

impl<'a, T: ?Sized, I: InterruptState> Deref for LockCellGuard<'a, T, I> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        unsafe {
            &*self.cell.val.get()
        }
    }
}

impl<'a, T: ?Sized, I: InterruptState> DerefMut for LockCellGuard<'a, T, I> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe {
            &mut *self.cell.val.get()
        }
    }
}

