// The main idea comes from cpython 3.8's `gcmodule.c` [1].
//
// [1]: https://github.com/python/cpython/blob/v3.8.0/Modules/gcmodule.c

// NOTE: Consider adding generation support if necessary. It won't be too hard.

use crate::cc::CcDyn;
use crate::cc::GcClone;
use crate::cc::GcHeader;
use crate::cc::GcHeaderWithExtras;
use crate::debug;
use crate::mutable_usize::Usize;
use crate::Cc;
use crate::Trace;
use std::cell::Cell;
use std::cell::RefCell;
use std::marker::PhantomData;
use std::mem;
use std::ops::Deref;
use std::pin::Pin;

/// A collection of [`Cc`](struct.Cc.html)s that might form cycles with one
/// another.
///
/// # Example
///
/// ```
/// use gcmodule::{Cc, CcObjectSpace, Trace};
/// use std::cell::RefCell;
///
/// let mut space = CcObjectSpace::default();
/// assert_eq!(space.count_tracked(), 0);
///
/// {
///     type List = Cc<RefCell<Vec<Box<dyn Trace>>>>;
///     let a: List = space.create(Default::default());
///     let b: List = space.create(Default::default());
///     a.borrow_mut().push(Box::new(b.clone()));
///     b.borrow_mut().push(Box::new(a.clone()));
/// }
///
/// assert_eq!(space.count_tracked(), 2);
/// assert_eq!(space.collect_cycles(), 2);
/// ```
///
/// Use [`Cc::new_in_space`](struct.Cc.html#method.new_in_space).
pub struct CcObjectSpace {
    /// Linked list to the tracked objects.
    pub(crate) list: RefCell<Pin<Box<GcHeader>>>,

    /// Mark `ObjectSpace` as `!Send` and `!Sync`. This enforces thread-exclusive
    /// access to the linked list so methods can use `&self` instead of
    /// `&mut self`, together with usage of interior mutability.
    _phantom: PhantomData<Cc<()>>,
}

/// This is a private type.
pub trait ObjectSpace: 'static + Sized {
    type RefCount: Usize;
    type Extras;

    /// Insert "header" and "value" to the linked list.
    fn insert(&self, header: &GcHeaderWithExtras<Self>, value: &dyn CcDyn);

    /// Remove from linked list.
    fn remove(header: &GcHeaderWithExtras<Self>);

    fn default_extras(&self) -> Self::Extras;
}

impl ObjectSpace for CcObjectSpace {
    type RefCount = Cell<usize>;
    type Extras = ();

    fn insert(&self, header: &GcHeaderWithExtras<Self>, value: &dyn CcDyn) {
        let header: &GcHeader = &header.gc_header;
        let prev: &GcHeader = &self.list.borrow();
        debug_assert!(header.next.get().is_null());
        let next = prev.next.get();
        header.prev.set(prev.deref());
        header.next.set(next);
        unsafe {
            // safety: The linked list is maintained, and pointers are valid.
            (&*next).prev.set(header);
            // safety: To access vtable pointer. Test by test_gc_header_value.
            let fat_ptr: [*mut (); 2] = mem::transmute(value);
            header.ccdyn_vptr.set(fat_ptr[1]);
        }
        prev.next.set(header);
    }

    #[inline]
    fn remove(header: &GcHeaderWithExtras<Self>) {
        let header: &GcHeader = &header.gc_header;
        debug_assert!(!header.next.get().is_null());
        debug_assert!(!header.prev.get().is_null());
        let next = header.next.get();
        let prev = header.prev.get();
        // safety: The linked list is maintained. Pointers in it are valid.
        unsafe {
            (*prev).next.set(next);
            (*next).prev.set(prev);
        }
        header.next.set(std::ptr::null_mut());
    }

    fn default_extras(&self) -> Self::Extras {
        ()
    }
}

impl Default for CcObjectSpace {
    /// Constructs an empty [`ObjectSpace`](struct.ObjectSpace.html).
    fn default() -> Self {
        let header = new_gc_list();
        Self {
            list: RefCell::new(header),
            _phantom: PhantomData,
        }
    }
}

impl CcObjectSpace {
    /// Count objects tracked by this [`ObjectSpace`](struct.ObjectSpace.html).
    pub fn count_tracked(&self) -> usize {
        let list: &GcHeader = &self.list.borrow();
        let mut count = 0;
        visit_list(list, |_| count += 1);
        count
    }

    /// Collect cyclic garbage tracked by this [`ObjectSpace`](struct.ObjectSpace.html).
    /// Return the number of objects collected.
    pub fn collect_cycles(&self) -> usize {
        let list: &GcHeader = &self.list.borrow();
        collect_list(list)
    }

    /// Constructs a new [`Cc<T>`](struct.Cc.html) in this
    /// [`ObjectSpace`](struct.ObjectSpace.html).
    ///
    /// The returned [`Cc<T>`](struct.Cc.html) can refer to other `Cc`s in the
    /// same [`ObjectSpace`](struct.ObjectSpace.html).
    ///
    /// If a `Cc` refers to another `Cc` in another
    /// [`ObjectSpace`](struct.ObjectSpace.html), the cyclic collector will not
    /// be able to collect cycles.
    pub fn create<T: Trace>(&self, value: T) -> Cc<T> {
        // `&mut self` ensures thread-exclusive access.
        Cc::new_in_space(value, self)
    }

    // TODO: Consider implementing "merge" or method to collect multiple spaces
    // together, to make it easier to support generational collection.
}

impl Drop for CcObjectSpace {
    fn drop(&mut self) {
        self.collect_cycles();
    }
}

/// Collect cyclic garbage in the current thread created by
/// [`Cc::new`](struct.Cc.html#method.new).
/// Return the number of objects collected.
pub fn collect_thread_cycles() -> usize {
    debug::log(|| ("collect", "collect_thread_cycles"));
    THREAD_OBJECT_SPACE.with(|list| list.collect_cycles())
}

/// Count number of objects tracked by the collector in the current thread
/// created by [`Cc::new`](struct.Cc.html#method.new).
/// Return the number of objects tracked.
pub fn count_thread_tracked() -> usize {
    THREAD_OBJECT_SPACE.with(|list| list.count_tracked())
}

thread_local!(pub(crate) static THREAD_OBJECT_SPACE: CcObjectSpace = CcObjectSpace::default());

/// Create an empty linked list with a dummy GcHeader.
fn new_gc_list() -> Pin<Box<GcHeader>> {
    let pinned = Box::pin(GcHeader::empty());
    let header: &GcHeader = pinned.deref();
    header.prev.set(header);
    header.next.set(header);
    pinned
}

/// Scan the specified linked list. Collect cycles.
fn collect_list(list: &GcHeader) -> usize {
    update_refs(list);
    subtract_refs(list);
    release_unreachable(list)
}

/// Visit the linked list.
fn visit_list<'a>(list: &'a GcHeader, mut func: impl FnMut(&'a GcHeader)) {
    // Skip the first dummy entry.
    let mut ptr = list.next.get();
    while ptr != list {
        // The linked list is maintained so the pointer is valid.
        let header: &GcHeader = unsafe { &*ptr };
        ptr = header.next.get();
        func(header);
    }
}

const PREV_MASK_COLLECTING: usize = 1;
const PREV_SHIFT: u32 = 1;

/// Temporarily use `GcHeader.prev` as `gc_ref_count`.
/// Idea comes from https://bugs.python.org/issue33597.
fn update_refs(list: &GcHeader) {
    visit_list(list, |header| {
        let ref_count = header.value().gc_ref_count();
        let shifted = (ref_count << PREV_SHIFT) | PREV_MASK_COLLECTING;
        header.prev.set(shifted as _);
    });
}

/// Subtract ref counts in `GcHeader.prev` by calling the non-recursive
/// `Trace::trace` on every track objects.
///
/// After this, potential unreachable objects will have ref count down
/// to 0. If vertexes in a connected component _all_ have ref count 0,
/// they are unreachable and can be released.
fn subtract_refs(list: &GcHeader) {
    let mut tracer = |header: &GcHeader| {
        if is_collecting(header) {
            debug_assert!(!is_unreachable(header));
            edit_gc_ref_count(header, -1);
        }
    };
    visit_list(list, |header| {
        header.value().gc_traverse(&mut tracer);
    });
}

/// Mark objects as reachable recursively. So ref count 0 means unreachable
/// values. This also removes the COLLECTING flag for reachable objects so
/// unreachable objects all have the COLLECTING flag set.
fn mark_reachable(list: &GcHeader) {
    fn revive(header: &GcHeader) {
        // hasn't visited?
        if is_collecting(header) {
            unset_collecting(header);
            if is_unreachable(header) {
                edit_gc_ref_count(header, 1); // revive
            }
            header.value().gc_traverse(&mut revive); // revive recursively
        }
    }
    visit_list(list, |header| {
        if is_collecting(header) && !is_unreachable(header) {
            unset_collecting(header);
            header.value().gc_traverse(&mut revive)
        }
    });
}

/// Release unreachable objects in the linked list.
fn release_unreachable(list: &GcHeader) -> usize {
    // Mark reachable objects. For example, A refers B. A's gc_ref_count
    // is 1 while B's gc_ref_count is 0. In this case B should be revived
    // by A's non-zero gc_ref_count.
    mark_reachable(list);

    let mut count = 0;

    // Count unreachable objects. This is an optimization to avoid realloc.
    visit_list(list, |header| {
        if is_unreachable(header) {
            count += 1;
        }
    });

    debug::log(|| ("collect", format!("{} unreachable objects", count)));

    // Build a list of what to drop. The collecting steps change the linked list
    // so `visit_list` cannot be used.
    //
    // Here we keep extra references to the `CcBox<T>` to keep them alive. This
    // ensures metadata fields like `ref_count` is available.
    let mut to_drop: Vec<Box<dyn GcClone>> = Vec::with_capacity(count);
    visit_list(list, |header| {
        if is_unreachable(header) {
            to_drop.push(header.value().gc_clone());
        }
    });

    // Restore "prev" so deleting nodes from the linked list can work.
    restore_prev(list);

    // Drop `T` without releasing memory of `CcBox<T>`. This might trigger some
    // recursive drops of other `Cc<T>`. `CcBox<T>` need to stay alive so
    // `Cc<T>::drop` can read the ref count metadata.
    for value in to_drop.iter() {
        value.gc_drop_t();
    }

    // At this point the only references to the `CcBox<T>`s are inside the
    // `to_drop` list. Dropping `to_drop` would release the memory.
    for value in to_drop.iter() {
        let ref_count = value.gc_ref_count();
        assert_eq!(
            ref_count, 1,
            concat!(
                "bug: unexpected ref-count after dropping cycles\n",
                "This usually indicates a buggy Trace or Drop implementation."
            )
        );
    }

    count
}

/// Restore `GcHeader.prev` as a pointer used in the linked list.
fn restore_prev(list: &GcHeader) {
    let mut prev = list;
    visit_list(list, |header| {
        header.prev.set(prev);
        prev = header;
    });
}

fn is_unreachable(header: &GcHeader) -> bool {
    let prev = header.prev.get() as usize;
    is_collecting(header) && (prev >> PREV_SHIFT) == 0
}

fn is_collecting(header: &GcHeader) -> bool {
    let prev = header.prev.get() as usize;
    (prev & PREV_MASK_COLLECTING) != 0
}

fn unset_collecting(header: &GcHeader) {
    let prev = header.prev.get() as usize;
    let new_prev = (prev & PREV_MASK_COLLECTING) ^ prev;
    header.prev.set(new_prev as _);
}

fn edit_gc_ref_count(header: &GcHeader, delta: isize) {
    let prev = header.prev.get() as isize;
    let new_prev = prev + (1 << PREV_SHIFT) * delta;
    header.prev.set(new_prev as _);
}
