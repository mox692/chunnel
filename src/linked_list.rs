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

    pub(crate) fn push_back(&mut self, node: &mut NodeType) {
        let cur_tail = &mut self.tail;
        let new_tail = unsafe { Some(NonNull::new_unchecked(node)) };
        if cur_tail.is_some() {
            unsafe { *cur_tail.unwrap().as_mut().next() = new_tail };
            self.tail = new_tail;
        } else {
            self.head = new_tail;
            self.tail = new_tail;
        }
    }

    pub(crate) fn pop_front(&mut self) -> Option<&mut NodeType> {
        let cur_head = self.head?;
        let cur_tail = self.tail.expect("Tail should not be null at this point.");
        if cur_tail.as_ptr() == cur_head.as_ptr() {
            let node = unsafe { Some(self.head.take().unwrap().as_mut()) };
            self.head = None;
            self.tail = None;

            node
        } else {
            let node = unsafe { self.head.take().unwrap().as_mut() };
            let next = node.next();
            self.head = *next;

            Some(node)
        }
    }
}
