use crate::debug;
use crate::{collect, Cc, Trace, Tracer};
use quickcheck::quickcheck;
use std::cell::RefCell;
use std::ops::Deref;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::SeqCst};
use std::sync::Arc;

#[test]
fn test_simple_untracked() {
    static DROPPED: AtomicBool = AtomicBool::new(false);
    struct X(&'static str);
    crate::untrack!(X);
    impl Drop for X {
        fn drop(&mut self) {
            DROPPED.store(true, SeqCst);
        }
    }
    {
        let v1 = Cc::new(X("abc"));
        {
            let v2 = v1.clone();
            assert_eq!(v1.deref().0, v2.deref().0);
        }
        assert!(!DROPPED.load(SeqCst));
    }
    assert!(DROPPED.load(SeqCst));
}

#[test]
fn test_simple_tracked() {
    static DROPPED: AtomicBool = AtomicBool::new(false);
    struct X(&'static str);
    impl Trace for X {}
    impl Drop for X {
        fn drop(&mut self) {
            DROPPED.store(true, SeqCst);
        }
    }
    {
        let v1 = Cc::new(X("abc"));
        {
            let v2 = v1.clone();
            assert_eq!(v1.deref().0, v2.deref().0);
        }
        assert!(!DROPPED.load(SeqCst));
    }
    assert!(DROPPED.load(SeqCst));
}

#[test]
fn test_simple_cycles() {
    assert_eq!(collect::collect_thread_cycles(), 0);
    {
        let a: Cc<RefCell<Vec<Box<dyn Trace>>>> = Cc::new(RefCell::new(Vec::new()));
        let b: Cc<RefCell<Vec<Box<dyn Trace>>>> = Cc::new(RefCell::new(Vec::new()));
        assert_eq!(collect::collect_thread_cycles(), 0);
        {
            let mut a = a.borrow_mut();
            a.push(Box::new(b.clone()));
        }
        {
            let mut b = b.borrow_mut();
            b.push(Box::new(a.clone()));
        }
        assert_eq!(collect::collect_thread_cycles(), 0);
    }
    assert_eq!(collect::collect_thread_cycles(), 2);
}

/// Track count of drop().
struct DropCounter<T>(T, Arc<AtomicUsize>);
impl<T: Trace> Trace for DropCounter<T> {
    fn trace(&self, tracer: &mut Tracer) {
        self.0.trace(tracer);
    }
}
impl<T> Drop for DropCounter<T> {
    fn drop(&mut self) {
        self.1.fetch_add(1, SeqCst);
    }
}

/// Test a graph of n (n <= 16) nodes, with specified edges between nodes.
fn test_small_graph(n: usize, edges: &[u8]) {
    assert!(n <= 16);
    let drop_count: Arc<AtomicUsize> = Arc::new(AtomicUsize::new(0));
    {
        let values: Vec<Cc<DropCounter<RefCell<Vec<Box<dyn Trace>>>>>> = (0..n)
            .map(|i| {
                debug::NEXT_DEBUG_NAME.with(|n| n.set(i));
                Cc::new(DropCounter(RefCell::new(Vec::new()), drop_count.clone()))
            })
            .collect();
        for &edge in edges {
            let from_index = ((edge as usize) >> 4) % n;
            let to_index = ((edge as usize) & 15) % n;
            let mut to = values[to_index].0.borrow_mut();
            to.push(Box::new(values[from_index].clone()));
        }
    }
    let drop_count_now = drop_count.load(SeqCst);
    assert_eq!(collect::collect_thread_cycles(), n - drop_count_now);
    assert_eq!(drop_count.load(SeqCst), n);
}

#[test]
fn test_drop_by_ref_count() {
    let log = debug::capture_log(|| test_small_graph(3, &[]));
    assert_eq!(
        log,
        r#"
0: track, clone (2), new
1: track, clone (2), new
2: track, clone (2), new
0: drop (1, tracked), untrack, drop (0)
1: drop (1, tracked), untrack, drop (0)
2: drop (1, tracked), untrack, drop (0)
collect: collect_thread_cycles, 0 unreachable objects"#
    );
}

#[test]
fn test_self_referential() {
    let log = debug::capture_log(|| test_small_graph(1, &[0x00, 0x00, 0x00]));
    assert_eq!(
        log,
        r#"
0: track, clone (2), new, clone (3), clone (4), clone (5), drop (4)
collect: collect_thread_cycles
0: gc_traverse, trace, trace, trace
collect: 1 unreachable objects
0: gc_prepare_drop, untrack, gc_force_drop
?: drop (ignored), drop (ignored), drop (ignored), gc_mark_for_release, drop (release)"#
    );
}

#[test]
fn test_3_object_cycle() {
    // 0 -> 1 -> 2 -> 0
    let log = debug::capture_log(|| test_small_graph(3, &[0x01, 0x12, 0x20]));
    assert_eq!(
        log,
        r#"
0: track, clone (2), new
1: track, clone (2), new
2: track, clone (2), new
0: clone (3)
1: clone (3)
2: clone (3)
0: drop (2)
1: drop (2)
2: drop (2)
collect: collect_thread_cycles
2: gc_traverse
1: trace, gc_traverse
0: trace, gc_traverse
2: trace
collect: 3 unreachable objects
2: gc_prepare_drop
1: gc_prepare_drop
0: gc_prepare_drop
2: untrack, gc_force_drop
?: drop (ignored)
1: untrack, gc_force_drop
?: drop (ignored)
0: untrack, gc_force_drop
?: drop (ignored), gc_mark_for_release, drop (release), gc_mark_for_release, drop (release), gc_mark_for_release, drop (release)"#
    );
}

#[test]
fn test_2_object_cycle_with_another_incoming_reference() {
    let log = debug::capture_log(|| test_small_graph(3, &[0x02, 0x20, 0x10]));
    assert_eq!(
        log,
        r#"
0: track, clone (2), new
1: track, clone (2), new
2: track, clone (2), new
0: clone (3)
2: clone (3)
1: clone (3)
0: drop (2)
1: drop (2)
2: drop (2)
collect: collect_thread_cycles
2: gc_traverse
0: trace
1: gc_traverse
0: gc_traverse
2: trace
1: trace
collect: 3 unreachable objects
2: gc_prepare_drop
1: gc_prepare_drop
0: gc_prepare_drop
2: untrack, gc_force_drop
?: drop (ignored)
1: untrack, gc_force_drop
0: untrack, gc_force_drop
?: drop (ignored), drop (ignored), gc_mark_for_release, drop (release), gc_mark_for_release, drop (release), gc_mark_for_release, drop (release)"#
    );
}

#[test]
fn test_2_object_cycle_with_another_outgoing_reference() {
    let log = debug::capture_log(|| test_small_graph(3, &[0x02, 0x20, 0x01]));
    assert_eq!(
        log,
        r#"
0: track, clone (2), new
1: track, clone (2), new
2: track, clone (2), new
0: clone (3)
2: clone (3)
0: clone (4), drop (3)
1: drop (1, tracked), untrack, drop (0)
0: drop (2)
2: drop (2)
collect: collect_thread_cycles
2: gc_traverse
0: trace, gc_traverse
2: trace
collect: 2 unreachable objects
2: gc_prepare_drop
0: gc_prepare_drop
2: untrack, gc_force_drop
?: drop (ignored)
0: untrack, gc_force_drop
?: drop (ignored), gc_mark_for_release, drop (release), gc_mark_for_release, drop (release)"#
    );
}

quickcheck! {
    fn test_quickcheck_16_vertex_graph(edges: Vec<u8>) -> bool {
        test_small_graph(16, &edges);
        true
    }
}