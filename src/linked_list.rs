//! Intrusive linked list.

use std::ptr::NonNull;

pub(crate) struct LinkedListHandle<NodeType: LinkedListNode> {
    pub(crate) head: Option<NonNull<NodeType>>,
    pub(crate) tail: Option<NonNull<NodeType>>,
}

pub(crate) trait LinkedListNode {
    fn next(&mut self) -> &mut Option<NonNull<Self>>;
}

impl<NodeType: LinkedListNode> LinkedListHandle<NodeType> {
    pub(crate) fn new() -> Self {
        Self {
            head: None,
            tail: None,
        }
    }

    /// Caller must ensure that:
    /// * `node` is non-null
    /// * this function is the sole owner of the linkedlist (e.g. wrap the entire list with mutex)
    pub(crate) fn push_back(&mut self, node: &mut NodeType) {
        let cur_tail = &mut self.tail;
        // SAFETY: `node` should not be null.
        let new_tail = unsafe { Some(NonNull::new_unchecked(node)) };
        if cur_tail.is_some() {
            // SAFETY: Since this function is the sole owner of the linkedlist, mutating the tail is fine.
            unsafe { *cur_tail.unwrap().as_mut().next() = new_tail };
            self.tail = new_tail;
        } else {
            self.head = new_tail;
            self.tail = new_tail;
        }
    }

    /// Caller must ensure that:
    /// * `node` is non-null
    /// * this function is the sole owner of the head pointer of the linkedlist
    pub(crate) fn pop_front(&mut self) -> Option<&mut NodeType> {
        let cur_head = self.head?;
        let cur_tail = self.tail.expect("Tail should not be null at this point.");
        if cur_tail.as_ptr() == cur_head.as_ptr() {
            // SAFETY: Since this function is the sole owner of the head pointer, getting a mutable
            //         reference is fine.
            let node = unsafe { Some(self.head.take().unwrap().as_mut()) };
            self.head = None;
            self.tail = None;

            node
        } else {
            // SAFETY: Since this function is the sole owner of the head pointer, getting a mutable
            //         reference is fine.
            let node = unsafe { self.head.take().unwrap().as_mut() };
            let next = node.next();
            self.head = *next;

            Some(node)
        }
    }
}
